//! Trusted local C2 experiment, never used by ordinary snapshot import.
//! Historical I/O failure is a dedicated panic caught ONLY around an isolated
//! candidate; it is never translated to a missing CRDT operation. The candidate
//! is discarded. This is not a new fallible public API or crash-safe database.
use super::*;
use crate::{
    encoding::ExportMode,
    kv_store::{KvStore, MemKvStore},
    sync::Mutex,
};
use loro_kv_store::mem_store::MemKvConfig;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs,
    ops::Bound,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    key: Vec<u8>,
    file: String,
    len: usize,
    checksum: u32,
}
#[derive(Serialize, Deserialize)]
struct Capture {
    version: u32,
    entries: Vec<Entry>,
    state: Entry,
    history_pairs: usize,
    mode: String,
    blocks: usize,
    block_bytes: usize,
    changes: usize,
}
#[derive(Clone, Default, Debug, Serialize)]
struct Counts {
    get: usize,
    scan: usize,
    reads: usize,
    bytes: usize,
    history_reads: usize,
    history_bytes: usize,
    clones: usize,
    keys: BTreeSet<Vec<u8>>,
}
#[derive(Debug)]
struct StorageFault(String);
#[derive(Clone, Debug)]
struct DiskKv {
    directory: PathBuf,
    entries: Arc<BTreeMap<Vec<u8>, Entry>>,
    overlay: BTreeMap<Vec<u8>, Option<Bytes>>,
    counts: Arc<Mutex<Counts>>,
    fault: Arc<Mutex<bool>>,
}
impl DiskKv {
    fn read(&self, key: &[u8], e: &Entry) -> Bytes {
        if *self.fault.lock() && key.len() == 12 {
            std::panic::panic_any(StorageFault("injected unavailable old block".into()));
        }
        let bytes = fs::read(self.directory.join(&e.file))
            .unwrap_or_else(|e| std::panic::panic_any(StorageFault(e.to_string())));
        if bytes.len() != e.len || xxhash_rust::xxh32::xxh32(&bytes, 0) != e.checksum {
            std::panic::panic_any(StorageFault("bad local block checksum".into()));
        }
        let mut c = self.counts.lock();
        c.reads += 1;
        c.bytes += bytes.len();
        c.keys.insert(key.to_vec());
        if key.len() == 12 {
            c.history_reads += 1;
            c.history_bytes += bytes.len();
        }
        bytes.into()
    }
    fn keys(&self) -> Vec<Vec<u8>> {
        let mut keys: BTreeSet<_> = self.entries.keys().cloned().collect();
        for (key, value) in &self.overlay {
            if value.is_some() {
                keys.insert(key.clone());
            } else {
                keys.remove(key);
            }
        }
        keys.into_iter().collect()
    }
}
impl KvStore for DiskKv {
    fn get(&self, key: &[u8]) -> Option<Bytes> {
        self.counts.lock().get += 1;
        if let Some(value) = self.overlay.get(key) {
            return value.clone();
        }
        self.entries.get(key).map(|e| self.read(key, e))
    }
    fn set(&mut self, key: &[u8], value: Bytes) {
        self.overlay.insert(key.to_vec(), Some(value));
    }
    fn compare_and_swap(&mut self, key: &[u8], old: Option<Bytes>, new: Bytes) -> bool {
        if self.get(key) != old {
            return false;
        }
        self.set(key, new);
        true
    }
    fn remove(&mut self, key: &[u8]) -> Option<Bytes> {
        let old = self.get(key);
        self.overlay.insert(key.to_vec(), None);
        old
    }
    fn contains_key(&self, key: &[u8]) -> bool {
        self.overlay
            .get(key)
            .map_or_else(|| self.entries.contains_key(key), Option::is_some)
    }
    fn scan(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Box<dyn DoubleEndedIterator<Item = (Bytes, Bytes)> + '_> {
        self.counts.lock().scan += 1;
        let keys = self
            .keys()
            .into_iter()
            .filter(|k| match start {
                Bound::Included(s) => k.as_slice() >= s,
                Bound::Excluded(s) => k.as_slice() > s,
                Bound::Unbounded => true,
            })
            .filter(|k| match end {
                Bound::Included(e) => k.as_slice() <= e,
                Bound::Excluded(e) => k.as_slice() < e,
                Bound::Unbounded => true,
            })
            .collect::<Vec<_>>();
        Box::new(keys.into_iter().map(move |key| {
            let value = self.get(&key).expect("indexed immutable entry");
            (Bytes::from(key), value)
        }))
    }
    fn len(&self) -> usize {
        self.keys().len()
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn size(&self) -> usize {
        self.keys()
            .iter()
            .map(|k| {
                self.overlay
                    .get(k)
                    .and_then(|v| v.as_ref())
                    .map_or_else(|| self.entries[k].len, Bytes::len)
            })
            .sum()
    }
    fn export_all(&mut self) -> Bytes {
        let mut kv = MemKvStore::new(MemKvConfig::default());
        for key in self.keys() {
            kv.set(&key, self.get(&key).unwrap());
        }
        kv.export_all()
    }
    fn import_all(&mut self, _bytes: Bytes) -> Result<(), String> {
        Err("test trusted disk backing cannot import arbitrary snapshot bytes".into())
    }
    fn clone_store(&self) -> Arc<Mutex<dyn KvStore>> {
        self.counts.lock().clones += 1;
        Arc::new(Mutex::new(self.clone()))
    }
}
fn save(dir: &Path, file: String, key: Vec<u8>, bytes: &[u8]) -> Entry {
    fs::write(dir.join(&file), bytes).unwrap();
    Entry {
        key,
        file,
        len: bytes.len(),
        checksum: xxhash_rust::xxh32::xxh32(bytes, 0),
    }
}
fn capture(dir: &Path, h: usize, mode: &str) {
    fs::create_dir_all(dir).unwrap();
    let doc = LoroDoc::new_auto_commit();
    doc.set_peer_id(1).unwrap();
    doc.set_change_merge_interval(0);
    doc.get_text("text")
        .insert_unicode(0, "abcdef玉🙂")
        .unwrap();
    doc.commit_then_renew();
    let old = doc.fork_cow().unwrap();
    old.set_peer_id(900_001).unwrap();
    let old_base = old.oplog_vv();
    old.get_text("text").insert_unicode(2, "OLD").unwrap();
    old.get_text("text").delete_unicode(5, 1).unwrap();
    old.commit_then_renew();
    let old_update = old
        .export(ExportMode::Updates {
            from: Cow::Owned(old_base),
        })
        .unwrap();
    for i in 0..h {
        if mode == "multi" {
            doc.set_peer_id(2 + (i % 4) as u64).unwrap();
        }
        let value = format!("{:08x}{}", i, "long-history-value".repeat(64));
        doc.get_text("text").insert_unicode(8, &value).unwrap();
        doc.commit_then_renew();
        doc.get_text("text")
            .delete_unicode(8, value.chars().count())
            .unwrap();
        doc.commit_then_renew();
    }
    // Current state removed a character still named by the old concurrent peer.
    doc.get_text("text").delete_unicode(1, 1).unwrap();
    doc.commit_then_renew();
    let full = doc.export(ExportMode::Snapshot).unwrap();
    let ordinary = LoroDoc::new_auto_commit();
    ordinary.import(&full).unwrap();
    assert_eq!(ordinary.get_deep_value(), doc.get_deep_value());
    let parsed = crate::encoding::parse_header_and_body(&full, true).unwrap();
    let split = _decode_snapshot_bytes(Bytes::copy_from_slice(parsed.body)).unwrap();
    assert!(split.shallow_root_state_bytes.is_empty());
    let mut kv = MemKvStore::new(MemKvConfig::default());
    kv.import_all(split.oplog_bytes).unwrap();
    let entries = kv
        .scan(Bound::Unbounded, Bound::Unbounded)
        .enumerate()
        .map(|(i, (k, v))| save(dir, format!("kv-{i}.bin"), k.to_vec(), &v))
        .collect::<Vec<_>>();
    let state = save(dir, "state.bin".into(), vec![], &split.state_bytes.unwrap());
    let cap = Capture {
        version: 1,
        blocks: entries.iter().filter(|e| e.key.len() == 12).count(),
        block_bytes: entries
            .iter()
            .filter(|e| e.key.len() == 12)
            .map(|e| e.len)
            .sum(),
        changes: doc.len_changes(),
        entries,
        state,
        history_pairs: h,
        mode: mode.into(),
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&cap).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("oracle.bin"), full).unwrap();
    fs::write(dir.join("old-update.bin"), old_update).unwrap();
}
fn open(dir: &Path) -> (LoroDoc, DiskKv, Capture) {
    let cap: Capture =
        serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(cap.version, 1);
    let kv = DiskKv {
        directory: dir.to_path_buf(),
        entries: Arc::new(
            cap.entries
                .iter()
                .map(|e| (e.key.clone(), e.clone()))
                .collect(),
        ),
        overlay: BTreeMap::new(),
        counts: Default::default(),
        fault: Default::default(),
    };
    let state = kv.read(&[], &cap.state);
    let doc = LoroDoc::new();
    {
        let mut oplog = doc.oplog().lock();
        let mut state_owner = doc.app_state().lock();
        state_owner.check_before_decode_snapshot().unwrap();
        assert!(state_owner.frontiers.is_empty());
        oplog
            .attach_trusted_history_backing(Arc::new(Mutex::new(kv.clone())))
            .unwrap();
        state_owner.store.decode(state).unwrap();
        state_owner
            .init_with_states_and_version(
                oplog.frontiers().clone(),
                &oplog,
                vec![],
                false,
                Default::default(),
            )
            .unwrap();
    }
    doc.start_auto_commit();
    assert!(!doc.is_shallow());
    (doc, kv, cap)
}
fn record(stage: &str, start: Instant, kv: &DiskKv, rows: &mut Vec<serde_json::Value>) {
    rows.push(serde_json::json!({"stage":stage,"ns":start.elapsed().as_nanos(),"cumulative":kv.counts.lock().clone()}));
}
fn oracle(dir: &Path) -> LoroDoc {
    let doc = LoroDoc::new_auto_commit();
    doc.import(&fs::read(dir.join("oracle.bin")).unwrap())
        .unwrap();
    doc
}
fn local_edit(doc: &LoroDoc) {
    doc.set_peer_id(800_001).unwrap();
    doc.get_text("text").insert_unicode(0, "LOCAL").unwrap();
    doc.commit_then_renew();
}
fn equivalent(a: &LoroDoc, b: &LoroDoc) {
    assert_eq!(a.get_deep_value(), b.get_deep_value());
    assert_eq!(a.oplog_vv(), b.oplog_vv());
    assert_eq!(a.state_frontiers(), b.state_frontiers());
    assert_eq!(
        serde_json::to_value(a.export_json_updates(&Default::default(), &a.oplog_vv(), false))
            .unwrap(),
        serde_json::to_value(b.export_json_updates(&Default::default(), &b.oplog_vv(), false))
            .unwrap()
    );
}
#[test]
#[ignore = "fresh-process trusted local history fixture; C2_DISK_PHASE author/read/fault"]
fn c2_trusted_disk_history_process() {
    let dir = PathBuf::from(std::env::var("C2_DISK_DIR").unwrap());
    let phase = std::env::var("C2_DISK_PHASE").unwrap();
    if phase == "author" {
        capture(
            &dir,
            std::env::var("C2_DISK_H").unwrap().parse().unwrap(),
            &std::env::var("C2_DISK_MODE").unwrap(),
        );
        return;
    }
    if phase == "fault" {
        fault_probe(&dir);
        return;
    }
    let start = Instant::now();
    let (owner, kv, cap) = open(&dir);
    let mut rows = Vec::new();
    record("attach", start, &kv, &mut rows);
    let start = Instant::now();
    let visible = owner.get_text("text").to_string();
    record("known_text", start, &kv, &mut rows);
    if phase == "enumerate" {
        let start = Instant::now();
        let _ = owner.get_deep_value();
        record("fresh_deep_value", start, &kv, &mut rows);
        let start = Instant::now();
        let _ = owner.export_json_updates(&Default::default(), &owner.oplog_vv(), false);
        record("fresh_history_query", start, &kv, &mut rows);
        fs::write(std::env::var("C2_DISK_OUTPUT").unwrap(), serde_json::to_vec_pretty(&serde_json::json!({"mode":cap.mode,"history_pairs":cap.history_pairs,"blocks":cap.blocks,"rows":rows,"separate_process_enumeration":true})).unwrap()).unwrap();
        return;
    }
    let start = Instant::now();
    let candidate = owner.fork_cow().unwrap();
    local_edit(&candidate);
    record("cow_first_edit", start, &kv, &mut rows);
    assert_eq!(owner.get_text("text").to_string(), visible); // cancellation/isolation, no oracle warming
    let provider_counts = |doc: &LoroDoc| {
        let oplog = doc.oplog().lock();
        serde_json::json!({"root_history_scans":oplog.change_store().root_history_scan_count_for_test(),"parsed_dag_nodes":oplog.dag.total_parsed_dag_node()})
    };
    let before_merge_provider = provider_counts(&candidate);
    let old = fs::read(dir.join("old-update.bin")).unwrap();
    let start = Instant::now();
    candidate.import(&old).unwrap();
    let after_merge_provider = provider_counts(&candidate);
    record("old_update", start, &kv, &mut rows);
    let start = Instant::now();
    let _ = candidate.get_deep_value();
    record("deep_value", start, &kv, &mut rows);
    let start = Instant::now();
    let _ = candidate.export_json_updates(&Default::default(), &candidate.oplog_vv(), false);
    record("full_history_query", start, &kv, &mut rows);
    // Only now instantiate complete controls and compare operation identities.
    let control = oracle(&dir);
    local_edit(&control);
    control.import(&old).unwrap();
    equivalent(&candidate, &control);
    // Reverse-order exchange is an additional convergence check, not in timed stages.
    let reverse = oracle(&dir);
    reverse.import(&old).unwrap();
    let local_only = owner.fork_cow().unwrap();
    local_edit(&local_only);
    let local_update = local_only
        .export(ExportMode::Updates {
            from: Cow::Owned(owner.oplog_vv()),
        })
        .unwrap();
    reverse.import(&local_update).unwrap();
    equivalent(&candidate, &reverse);
    let before_export = kv.counts.lock().clone();
    let full = candidate.export(ExportMode::Snapshot).unwrap();
    let reopened = LoroDoc::new_auto_commit();
    reopened.import(&full).unwrap();
    equivalent(&candidate, &reopened);
    let result = serde_json::json!({"before_merge_provider":before_merge_provider,"after_merge_provider":after_merge_provider,"history_pairs":cap.history_pairs,"mode":cap.mode,"blocks":cap.blocks,"block_bytes":cap.block_bytes,"changes":cap.changes,"manifest_bytes":fs::metadata(dir.join("manifest.json")).unwrap().len(),"state_bytes":cap.state.len,"rows":rows,"before_full_export":before_export,"after_full_export":kv.counts.lock().clone(),"full_oracle_equal":true,"reverse_merge_equal":true,"snapshot_reopen_equal":true,"shallow":false,"trusted_local_capture_only":true,"checksum_not_authentication":true});
    fs::write(
        std::env::var("C2_DISK_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
}
fn fault_probe(dir: &Path) {
    let (owner, kv, cap) = open(dir);
    assert!(
        cap.blocks > 1,
        "fault fixture must have multiple old blocks"
    );
    let before = owner.get_text("text").to_string();
    let vv = owner.oplog_vv();
    let candidate = owner.fork_cow().unwrap();
    *kv.fault.lock() = true;
    let result = catch_unwind(AssertUnwindSafe(|| {
        candidate
            .oplog()
            .lock()
            .change_store()
            .visit_all_changes(&mut |_| {});
    }));
    match result {
        Err(e) => {
            if let Some(fault) = e.downcast_ref::<StorageFault>() {
                assert!(!fault.0.is_empty());
            } else {
                std::panic::resume_unwind(e);
            }
        }
        Ok(_) => panic!("old history query did not fetch a block"),
    }
    drop(candidate);
    *kv.fault.lock() = false;
    assert_eq!(owner.get_text("text").to_string(), before);
    assert_eq!(owner.oplog_vv(), vv);
    let retry = owner.fork_cow().unwrap();
    retry
        .import(&fs::read(dir.join("old-update.bin")).unwrap())
        .unwrap();
    let control = oracle(dir);
    control
        .import(&fs::read(dir.join("old-update.bin")).unwrap())
        .unwrap();
    equivalent(&retry, &control);
    // Corrupt/missing historical body faults are caught on a disposable branch.
    let history = cap.entries.iter().find(|e| e.key.len() == 12).unwrap();
    let history_path = dir.join(&history.file);
    let history_saved = fs::read(&history_path).unwrap();
    for missing in [false, true] {
        if missing {
            fs::remove_file(&history_path).unwrap();
        } else {
            fs::write(&history_path, b"corrupt old block").unwrap();
        }
        let branch = owner.fork_cow().unwrap();
        let bad = catch_unwind(AssertUnwindSafe(|| {
            branch
                .oplog()
                .lock()
                .change_store()
                .visit_all_changes(&mut |_| {})
        }));
        fs::write(&history_path, &history_saved).unwrap();
        assert!(bad.err().unwrap().is::<StorageFault>());
        drop(branch);
        assert_eq!(owner.get_text("text").to_string(), before);
        assert_eq!(owner.oplog_vv(), vv);
    }
    // Independent corrupt/missing backing probes fail during the attach's real reads.
    let entry = cap.entries.iter().find(|e| e.key == b"vv").unwrap();
    let path = dir.join(&entry.file);
    let saved = fs::read(&path).unwrap();
    fs::write(&path, b"bad").unwrap();
    let bad = catch_unwind(AssertUnwindSafe(|| open(dir)));
    fs::write(&path, &saved).unwrap();
    assert!(bad.err().unwrap().is::<StorageFault>());
    fs::remove_file(&path).unwrap();
    let missing = catch_unwind(AssertUnwindSafe(|| open(dir)));
    fs::write(&path, &saved).unwrap();
    assert!(missing.err().unwrap().is::<StorageFault>());
    fs::write(std::env::var("C2_DISK_OUTPUT").unwrap(),serde_json::to_vec_pretty(&serde_json::json!({"isolated_storage_fault_refused":true,"accepted_owner_unchanged":true,"retry_with_available_history_equal":true,"bad_checksum_refused":true,"missing_key_body_refused":true,"public_fallible_api":false,"historical_missing_and_corrupt_refused":true})).unwrap()).unwrap();
}
