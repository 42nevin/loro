//! TEST-ONLY adapter for actual Loro full-history capture and lazy reopening.
//! Binary objects use a hex fixture envelope because Store payloads are UTF-8.
//! This intentionally copies/encodes bytes and is not a zero-copy/perf design.
use super::*;
use loro::{guard_trusted_history, HistoryEntry, HistoryValue, LoroDoc, TrustedHistoryCapture, TrustedHistorySource};
use std::sync::atomic::AtomicBool;

type SharedStore = Arc<Mutex<RootedStore>>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BlobRef {
    object: u64,
    bytes_len: usize,
    digest: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentManifest {
    version: u64,
    state: BlobRef,
    history: Vec<(Vec<u8>, BlobRef)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootEnvelope { version: u64, manifest: BlobRef }
#[derive(Debug)]
struct Published {
    root: u64,
    retained: usize,
    written: usize,
}

fn hex(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8] = b"0123456789abcdef";
    bytes.iter().flat_map(|b| [DIGITS[(b >> 4) as usize], DIGITS[(b & 15) as usize]]).collect()
}
fn unhex(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.len() % 2 == 0, "odd fixture hex length");
    let digit = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(anyhow::anyhow!("invalid fixture hex")),
    };
    bytes.chunks_exact(2).map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?)).collect()
}
fn write_bytes(store: &mut RootedStore, bytes: &[u8]) -> Result<Vec<Descriptor>> {
    let encoded = hex(bytes);
    let chunks = encoded.chunks(PAGE - HEADER).collect::<Vec<_>>();
    let lengths = chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>();
    let batch = store.allocate_batch(&lengths, &mut |index, out| out.copy_from_slice(chunks[index]))?;
    Ok(batch.into_iter().map(|(descriptor, _lease)| descriptor).collect())
}
fn write_blob(store: &mut RootedStore, bytes: &[u8]) -> Result<BlobRef> {
    let extents = write_bytes(store, bytes)?;
    let object = store.object(vec![], extents)?;
    Ok(BlobRef { object, bytes_len: bytes.len(), digest: *blake3::hash(bytes).as_bytes() })
}
fn read_object_bytes(store: &RootedStore, pin: &RootPin, object: u64) -> Result<Vec<u8>> {
    let object = store.manifest.objects.get(&object).ok_or_else(|| anyhow::anyhow!("missing blob object"))?;
    let mut encoded = Vec::new();
    for descriptor in &object.extents {
        encoded.extend_from_slice(&store.read(pin, descriptor.id)?);
    }
    unhex(&encoded)
}
fn read_blob(store: &RootedStore, pin: &RootPin, reference: &BlobRef) -> Result<Vec<u8>> {
    let object = store.manifest.objects.get(&reference.object).ok_or_else(|| anyhow::anyhow!("missing blob object"))?;
    ensure!(object.children.is_empty(), "blob object cannot contain child edges");
    let bytes = read_object_bytes(store, pin, reference.object)?;
    ensure!(bytes.len() == reference.bytes_len && blake3::hash(&bytes).as_bytes() == &reference.digest,
        "immutable object commitment mismatch");
    Ok(bytes)
}

struct DiskSource {
    store: SharedStore,
    pin: RootPin,
    history: BTreeMap<Vec<u8>, BlobRef>,
    reads: Mutex<BTreeMap<Vec<u8>, usize>>,
    fail_reads: AtomicBool,
}
impl std::fmt::Debug for DiskSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskSource").field("history_keys", &self.history.len()).finish()
    }
}
impl DiskSource {
    fn read_count(&self) -> usize { self.reads.lock().unwrap().values().sum() }
    fn unread_keys(&self) -> usize {
        let reads = self.reads.lock().unwrap();
        self.history.keys().filter(|key| !reads.contains_key(*key)).count()
    }
}
impl TrustedHistorySource for DiskSource {
    fn read(&self, key: &[u8]) -> std::result::Result<Vec<u8>, String> {
        *self.reads.lock().map_err(|_| "read counter poisoned")?.entry(key.to_vec()).or_default() += 1;
        if self.fail_reads.load(Ordering::SeqCst) { return Err("injected history source failure".into()); }
        let reference = self.history.get(key).ok_or_else(|| "missing committed history key".to_string())?;
        let store = self.store.lock().map_err(|_| "store controller mutex poisoned")?;
        read_blob(&store, &self.pin, reference).map_err(|error| error.to_string())
    }
}

struct BackedDoc {
    doc: LoroDoc,
    // Always the ORIGINAL attached source, even after a capture is published.
    // A successful publication does not silently rebase this document.
    source: Arc<DiskSource>,
}
impl BackedDoc {
    fn fork(&self) -> Result<Self> {
        let doc = guard_trusted_history(|| self.doc.fork_cow())??;
        Ok(Self { doc, source: self.source.clone() })
    }
    fn publish(&self, kind: RootKind) -> Result<Published> {
        let capture = guard_trusted_history(|| self.doc.capture_trusted_history())??;
        ensure!(!capture.bootstrap_full_export, "backed publication unexpectedly materialized full history");
        persist_capture(&self.source.store, capture, Some(&self.source), kind)
    }
}

fn persist_capture(shared: &SharedStore, capture: TrustedHistoryCapture, original: Option<&Arc<DiskSource>>, kind: RootKind) -> Result<Published> {
    ensure!(capture.bootstrap_full_export == original.is_none(), "capture/backing provenance mismatch");
    if let Some(source) = original { ensure!(Arc::ptr_eq(shared, &source.store), "foreign original source"); }
    let mut seen = BTreeSet::new();
    for (key, value) in &capture.history {
        ensure!(seen.insert(key.clone()), "duplicate captured history key");
        if let HistoryValue::Retained { bytes_len } = value {
            let reference = original.and_then(|source| source.history.get(key))
                .ok_or_else(|| anyhow::anyhow!("retained key is absent from original source"))?;
            ensure!(reference.bytes_len == *bytes_len, "retained length differs from original source");
        }
    }
    ensure!(seen.contains(b"vv".as_slice()) && seen.contains(b"fr".as_slice()), "capture missing full-history directory metadata");
    // No Loro call or source read may run with this mutex held. Source callbacks
    // acquire it themselves; capture was completed before taking the lock.
    let mut store = shared.lock().unwrap();
    let state = write_blob(&mut store, &capture.state)?;
    let mut history = Vec::new();
    let mut retained = 0;
    let mut written = 0;
    for (key, value) in capture.history {
        let reference = match value {
            HistoryValue::Written(bytes) => { written += 1; write_blob(&mut store, &bytes)? },
            HistoryValue::Retained { .. } => {
                retained += 1;
                original.unwrap().history[&key].clone()
            }
        };
        history.push((key, reference));
    }
    let manifest = DocumentManifest { version: 1, state, history };
    let committed_manifest = write_blob(&mut store, &serde_json::to_vec(&manifest)?)?;
    let children = [manifest.state.object, committed_manifest.object].into_iter()
        .chain(manifest.history.iter().map(|(_, reference)| reference.object))
        .collect::<BTreeSet<_>>().into_iter().collect();
    let envelope = RootEnvelope { version: 1, manifest: committed_manifest };
    let extents = write_bytes(&mut store, &serde_json::to_vec(&envelope)?)?;
    let object = store.object(children, extents)?;
    let root = store.root(kind, object)?;
    Ok(Published { root, retained, written })
}

fn open_document(shared: &SharedStore, root_id: u64) -> Result<BackedDoc> {
    let (pin, manifest, state) = {
        let store = shared.lock().unwrap();
        let pin = store.pin(root_id)?;
        let object_id = store.manifest.roots[&root_id].object;
        let bytes = read_object_bytes(&store, &pin, object_id)?;
        let envelope: RootEnvelope = serde_json::from_slice(&bytes)?;
        ensure!(envelope.version == 1, "unsupported Loro root envelope");
        let manifest_bytes = read_blob(&store, &pin, &envelope.manifest)?;
        let manifest: DocumentManifest = serde_json::from_slice(&manifest_bytes)?;
        ensure!(manifest.version == 1, "unsupported Loro root manifest version");
        let required: BTreeSet<_> = [manifest.state.object, envelope.manifest.object].into_iter()
            .chain(manifest.history.iter().map(|(_, reference)| reference.object)).collect();
        let actual: BTreeSet<_> = store.manifest.objects[&object_id].children.iter().copied().collect();
        ensure!(actual == required, "Loro root graph and capture directory disagree");
        let keys: BTreeSet<_> = manifest.history.iter().map(|(key, _)| key).collect();
        ensure!(keys.len() == manifest.history.len(), "duplicate persisted history keys");
        let state = read_blob(&store, &pin, &manifest.state)?;
        (pin, manifest, state)
    }; // Release controller lock BEFORE open_trusted_history invokes the source.
    let entries = manifest.history.iter().map(|(key, reference)| HistoryEntry { key: key.clone(), bytes_len: reference.bytes_len }).collect();
    let source = Arc::new(DiskSource {
        store: shared.clone(), pin, history: manifest.history.into_iter().collect(),
        reads: Mutex::new(BTreeMap::new()), fail_reads: AtomicBool::new(false),
    });
    let doc = LoroDoc::open_trusted_history(&state, entries, source.clone())?;
    ensure!(!doc.is_shallow(), "trusted history must remain complete");
    Ok(BackedDoc { doc, source })
}

fn bootstrap(shared: &SharedStore, doc: &LoroDoc, kind: RootKind) -> Result<Published> {
    let capture = doc.capture_trusted_history()?;
    ensure!(capture.bootstrap_full_export, "bootstrap requires an unbacked document");
    persist_capture(shared, capture, None, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::reclamation_probe::reuse_pages;
    use loro::{ExportMode, Frontiers};

    #[derive(Serialize, Deserialize)]
    struct Oracle {
        original_snapshot: Vec<u8>,
        merged_snapshot: Vec<u8>,
        historical_cut: Vec<u8>,
        historical_text: String,
        old_update: Vec<u8>,
        root0: u64,
        root1: Option<u64>,
        root2: Option<u64>,
    }
    fn create_author() -> (LoroDoc, Frontiers, String, Vec<u8>) {
        let doc = LoroDoc::new();
        doc.set_peer_id(1).unwrap();
        doc.get_text("text").insert(0, "abcdef").unwrap();
        doc.commit();
        let old_device = doc.fork_cow().unwrap();
        old_device.set_peer_id(9100).unwrap();
        let base = old_device.oplog_vv();
        old_device.get_text("text").insert(2, "OLD").unwrap();
        old_device.commit();
        let update = old_device.export(ExportMode::updates(&base)).unwrap();
        let mut historical_cut = doc.oplog_frontiers();
        let mut historical_text = String::new();
        for i in 0..32 {
            doc.set_peer_id(2 + i % 4).unwrap();
            let payload = format!("deleted-history-{i:02}|").repeat(128);
            doc.get_text("text").insert(6, &payload).unwrap();
            doc.commit();
            if i == 0 {
                historical_cut = doc.oplog_frontiers();
                historical_text = format!("abcdef{payload}");
            }
            doc.get_text("text").delete(6, payload.len()).unwrap();
            doc.commit();
        }
        assert_eq!(doc.get_text("text").to_string(), "abcdef");
        (doc, historical_cut, historical_text, update)
    }
    fn shared_open(path: &Path) -> SharedStore {
        Arc::new(Mutex::new(unsafe { RootedStore::open(path).unwrap() }))
    }
    fn snapshot_doc(bytes: &[u8]) -> LoroDoc {
        let doc = LoroDoc::new();
        doc.import(bytes).unwrap();
        doc
    }
    fn append_current_edit(doc: &LoroDoc) {
        doc.set_peer_id(9000).unwrap();
        doc.get_text("text").insert(0, "N").unwrap();
        doc.commit();
    }
    fn equal_docs(actual: &LoroDoc, expected: &LoroDoc) {
        assert_eq!(actual.get_deep_value(), expected.get_deep_value());
        assert_eq!(actual.oplog_vv(), expected.oplog_vv());
        assert_eq!(actual.oplog_frontiers(), expected.oplog_frontiers());
        assert!(!actual.is_shallow());
    }
    fn normalized_history(doc: &LoroDoc) -> serde_json::Value {
        let mut history = doc.export_json_updates_without_peer_compression(&loro::VersionVector::default(), &doc.oplog_vv());
        history.changes.sort_by_key(|change| (change.id.peer, change.id.counter));
        for change in &mut history.changes {
            change.deps.sort_by_key(|id| (id.peer, id.counter));
        }
        serde_json::to_value(history).unwrap()
    }
    fn payload_set(store: &RootedStore, root: u64) -> BTreeMap<u64, Descriptor> {
        store.trace([store.manifest.roots[&root].object]).unwrap()
    }
    fn oracle_path(path: &Path) -> std::path::PathBuf { path.with_extension("oracle.json") }
    fn write_oracle(path: &Path, oracle: &Oracle) {
        std::fs::write(oracle_path(path), serde_json::to_vec(oracle).unwrap()).unwrap();
    }
    fn read_oracle(path: &Path) -> Oracle {
        serde_json::from_slice(&std::fs::read(oracle_path(path)).unwrap()).unwrap()
    }

    #[test]
    fn actual_loro_history_gc_across_four_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loro-data");
        for phase in ["bootstrap", "branch-and-retire", "historical-checkout-and-sync", "verify-and-release"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "store::rooted_reclamation::loro_integration::tests::loro_process_phase", "--nocapture"])
                .env("LORO_REAL_GC_PATH", &path).env("LORO_REAL_GC_PHASE", phase)
                .output().unwrap();
            assert!(output.status.success(), "phase {phase}:\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
    }

    #[test]
    fn loro_process_phase() {
        let Ok(phase) = std::env::var("LORO_REAL_GC_PHASE") else { return; };
        let path = std::path::PathBuf::from(std::env::var_os("LORO_REAL_GC_PATH").unwrap());
        match phase.as_str() {
            "bootstrap" => {
                let shared = Arc::new(Mutex::new(unsafe { RootedStore::create(&path).unwrap() }));
                let (doc, cut, historical_text, old_update) = create_author();
                let original_snapshot = doc.export(ExportMode::Snapshot).unwrap();
                let published = bootstrap(&shared, &doc, RootKind::History).unwrap();
                assert!(published.written > 8);
                assert_eq!(published.retained, 0);
                write_oracle(&path, &Oracle { original_snapshot, merged_snapshot: vec![], historical_cut: cut.encode(), historical_text, old_update, root0: published.root, root1: None, root2: None });
                println!("MEASUREMENT {{\"scenario\":\"real_loro_bootstrap\",\"actual_history_keys\":{},\"visible_text_bytes\":6,\"deleted_history_rounds\":32}}", published.written);
            }
            "branch-and-retire" => {
                let shared = shared_open(&path);
                let mut oracle = read_oracle(&path);
                let accepted = open_document(&shared, oracle.root0).unwrap();
                assert_eq!(accepted.doc.get_text("text").to_string(), "abcdef");
                assert!(accepted.source.unread_keys() > 8);
                let old_history = accepted.source.history.clone();
                let branch = accepted.fork().unwrap();
                guard_trusted_history(|| append_current_edit(&branch.doc)).unwrap();
                let before = branch.source.read_count();
                let published = branch.publish(RootKind::Current).unwrap();
                assert_eq!(branch.source.read_count(), before, "real backed capture/persistence must not fetch retained bodies");
                assert!(published.retained > 8);
                assert!(published.written > 0);
                let latest = open_document(&shared, published.root).unwrap();
                let actually_shared = latest.source.history.iter().filter(|(key, reference)| old_history.get(*key) == Some(*reference)).count();
                assert_eq!(actually_shared, published.retained);
                equal_docs(&latest.doc, &branch.doc);
                let (old_only, old_pages) = {
                    let mut store = shared.lock().unwrap();
                    let old = payload_set(&store, oracle.root0);
                    let new = payload_set(&store, published.root);
                    let old_only = old.keys().filter(|id| !new.contains_key(id)).copied().collect::<BTreeSet<_>>();
                    let old_pages = old_only.iter().map(|id| old[id].page).collect::<BTreeSet<_>>();
                    assert!(!old_only.is_empty());
                    store.retire(oracle.root0).unwrap();
                    assert!(store.sweep().unwrap().pages.is_empty());
                    (old_only, old_pages)
                };
                drop(accepted);
                let BackedDoc { doc: branch, source: explicit_source } = branch;
                drop(explicit_source);
                // No adapter-owned source Arc remains: the actual Loro CoW
                // document's internal backing alone owns the old source pin.
                assert!(shared.lock().unwrap().sweep().unwrap().pages.is_empty());
                guard_trusted_history(|| branch.get_text("text").insert(1, "private")).unwrap().unwrap();
                guard_trusted_history(|| branch.commit()).unwrap();
                assert_eq!(branch.get_text("text").to_string(), "Nprivateabcdef");
                drop(branch); // its unpersisted edit is abandoned, root1 remains Nabcdef
                let sweep = shared.lock().unwrap().sweep().unwrap();
                assert_eq!(sweep.retired_ids, old_only);
                assert_eq!(sweep.pages, old_pages);
                reuse_pages(&mut shared.lock().unwrap().store, &sweep.pages);
                assert_eq!(latest.doc.get_text("text").to_string(), "Nabcdef");
                oracle.root1 = Some(published.root);
                write_oracle(&path, &oracle);
                println!("MEASUREMENT {{\"scenario\":\"real_loro_cow_root_retirement\",\"retained_original_history_objects\":{},\"written_history_objects\":{},\"capture_source_read_delta\":0,\"old_branch_only_blocks_collection\":true,\"old_root_exclusive_pages_reused\":{},\"shared_history_preserved\":true}}", published.retained, published.written, sweep.pages.len());
            }
            "historical-checkout-and-sync" => {
                let shared = shared_open(&path);
                let mut oracle = read_oracle(&path);
                let backed = open_document(&shared, oracle.root1.unwrap()).unwrap();
                let unread = backed.source.unread_keys();
                assert!(unread > 8);
                // This alternate snapshot is an independent oracle only. The
                // tested document was already restored from the reclaimed store.
                let reference = snapshot_doc(&oracle.original_snapshot);
                append_current_edit(&reference);
                equal_docs(&backed.doc, &reference);
                let before_checkout = backed.source.read_count();
                let cut = Frontiers::decode(&oracle.historical_cut).unwrap();
                guard_trusted_history(|| backed.doc.checkout(&cut)).unwrap().unwrap();
                assert_eq!(backed.doc.get_text("text").to_string(), oracle.historical_text);
                assert!(backed.source.read_count() > before_checkout);
                guard_trusted_history(|| backed.doc.checkout_to_latest()).unwrap();
                equal_docs(&backed.doc, &reference);
                guard_trusted_history(|| backed.doc.import(&oracle.old_update)).unwrap().unwrap();
                reference.import(&oracle.old_update).unwrap();
                equal_docs(&backed.doc, &reference);
                let before_capture = backed.source.read_count();
                let merged = backed.publish(RootKind::Current).unwrap();
                assert_eq!(backed.source.read_count(), before_capture);
                let fresh = open_document(&shared, merged.root).unwrap();
                equal_docs(&fresh.doc, &reference);
                shared.lock().unwrap().retire(oracle.root1.unwrap()).unwrap();
                assert!(shared.lock().unwrap().sweep().unwrap().pages.is_empty());
                drop(backed);
                let swept = shared.lock().unwrap().sweep().unwrap();
                assert!(!swept.pages.is_empty());
                reuse_pages(&mut shared.lock().unwrap().store, &swept.pages);
                oracle.root2 = Some(merged.root);
                oracle.merged_snapshot = reference.export(ExportMode::Snapshot).unwrap();
                write_oracle(&path, &oracle);
                println!("MEASUREMENT {{\"scenario\":\"real_loro_history_after_gc\",\"fresh_process\":true,\"unread_history_keys_at_open\":{unread},\"deleted_text_recovered_at_old_frontier\":true,\"old_device_sync_matches_native_oracle\":true,\"superseded_root1_pages_reused\":{},\"merged_retained_history_objects\":{}}}", swept.pages.len(), merged.retained);
            }
            "verify-and-release" => {
                let shared = shared_open(&path);
                let oracle = read_oracle(&path);
                let backed = open_document(&shared, oracle.root2.unwrap()).unwrap();
                let before = backed.source.read_count();
                let snapshot = guard_trusted_history(|| backed.doc.export(ExportMode::Snapshot)).unwrap().unwrap();
                assert!(backed.source.read_count() > before);
                let reconstructed = snapshot_doc(&snapshot);
                let reference = snapshot_doc(&oracle.merged_snapshot);
                equal_docs(&reconstructed, &reference);
                let actual_history = guard_trusted_history(|| normalized_history(&backed.doc)).unwrap();
                assert_eq!(actual_history, normalized_history(&reference));
                assert_eq!(normalized_history(&reconstructed), normalized_history(&reference));
                let cut = Frontiers::decode(&oracle.historical_cut).unwrap();
                guard_trusted_history(|| backed.doc.checkout(&cut)).unwrap().unwrap();
                assert_eq!(backed.doc.get_text("text").to_string(), oracle.historical_text);
                guard_trusted_history(|| backed.doc.checkout_to_latest()).unwrap();
                shared.lock().unwrap().retire(oracle.root2.unwrap()).unwrap();
                assert!(shared.lock().unwrap().sweep().unwrap().pages.is_empty());
                drop(backed);
                let swept = shared.lock().unwrap().sweep().unwrap();
                assert!(!swept.pages.is_empty());
                reuse_pages(&mut shared.lock().unwrap().store, &swept.pages);
                drop(shared);
                let reopened = shared_open(&path);
                assert!(open_document(&reopened, oracle.root2.unwrap()).is_err());
                assert!(reopened.lock().unwrap().sweep().unwrap().pages.is_empty());
                println!("MEASUREMENT {{\"scenario\":\"real_loro_final_root_release\",\"native_export_state_vv_frontiers_equal\":true,\"full_json_history_equal\":true,\"historical_checkout_still_complete\":true,\"final_payload_pages_reused\":{},\"root_retirement_explicit\":true}}", swept.pages.len());
            }
            _ => panic!("unknown phase"),
        }
    }

    #[test]
    fn actual_loro_source_failure_cannot_publish_and_commitments_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failure");
        let shared = Arc::new(Mutex::new(unsafe { RootedStore::create(&path).unwrap() }));
        let (doc, _, _, _) = create_author();
        let published = bootstrap(&shared, &doc, RootKind::Current).unwrap();
        let accepted = open_document(&shared, published.root).unwrap();
        let failed = accepted.fork().unwrap();
        let root_count = shared.lock().unwrap().manifest.next_root;
        failed.source.fail_reads.store(true, Ordering::SeqCst);
        assert!(guard_trusted_history(|| failed.doc.export(ExportMode::Snapshot)).is_err());
        assert!(failed.publish(RootKind::Current).is_err());
        assert_eq!(shared.lock().unwrap().manifest.next_root, root_count);
        drop(failed);
        accepted.source.fail_reads.store(false, Ordering::SeqCst);
        assert_eq!(accepted.doc.get_text("text").to_string(), "abcdef");
        guard_trusted_history(|| accepted.doc.export(ExportMode::Snapshot)).unwrap().unwrap();
        let (key, reference) = accepted.source.history.iter().next().unwrap();
        let mut wrong = reference.clone();
        wrong.digest[0] ^= 1;
        let store = shared.lock().unwrap();
        assert!(read_blob(&store, &accepted.source.pin, &wrong).is_err());
        let actual = read_blob(&store, &accepted.source.pin, reference).unwrap();
        assert_eq!(actual.len(), reference.bytes_len);
        assert!(!key.is_empty());
        println!("MEASUREMENT {{\"scenario\":\"real_loro_source_failure\",\"failed_candidate_discarded\":true,\"new_root_published\":false,\"accepted_owner_still_usable\":true,\"digest_mismatch_rejected\":true}}");
    }
}
