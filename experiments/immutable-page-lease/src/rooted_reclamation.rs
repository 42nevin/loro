//! TEST-ONLY versioned reachability and permanently issued identities.
//! This controller owns its Store exclusively. Production Store APIs are not
//! changed, old opaque schemas are not migrated, and metadata is never collected.
use super::*;
use super::reclamation_probe::{sweep_with_manifest, Sweep};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MANIFEST_ROOT: usize = 5;
const MANIFEST_MAGIC: u64 = 0x5245434c41494d31;
const MANIFEST_FORMAT: &str = "rooted-reclamation";
const MANIFEST_VERSION: u64 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    children: Vec<u64>,
    extents: Vec<Descriptor>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum RootKind { Current, History, EditingBranch }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    kind: RootKind,
    object: u64,
    active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    version: u64,
    sequence: u64,
    store_id: [u8; 16],
    next_extent: u64,
    next_object: u64,
    next_root: u64,
    objects: BTreeMap<u64, Object>,
    roots: BTreeMap<u64, Root>,
}

struct RootPinInner {
    object: u64,
    pins: Arc<Mutex<BTreeMap<u64, usize>>>,
    // A pin may outlive the facade. Keep exclusion locks until the last reader
    // dies so reopening cannot lose the pin registry and collect unread objects.
    owner: Arc<Owner>,
}
impl Drop for RootPinInner {
    fn drop(&mut self) {
        let mut pins = self.pins.lock().unwrap();
        let count = pins.get_mut(&self.object).expect("registered root pin");
        *count -= 1;
        if *count == 0 { pins.remove(&self.object); }
    }
}
#[derive(Clone)]
struct RootPin(Arc<RootPinInner>);

// Non-Clone linear capability. Only reserve_extent constructs one; consuming
// it (including a failed allocation) permanently burns its durable identity.
struct ReservedExtent { id: u64, store_id: [u8; 16] }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitSite { Manifest, Payload, Sweep }

struct RootedStore {
    // Never return this Store or an unrestricted insertion interface to callers.
    store: Store,
    manifest: Manifest,
    pins: Arc<Mutex<BTreeMap<u64, usize>>>,
    poisoned: bool,
    payload_reads: AtomicU64,
    // Test injection: actual commit succeeds, but caller receives no success.
    lost_ack: Option<CommitSite>,
}

impl RootedStore {
    unsafe fn create(path: &Path) -> Result<Self> {
        ensure!(!path.exists(), "rooted fixture creation requires a fresh path");
        let mut store = Store::open(path)?;
        let manifest = Manifest {
            format: MANIFEST_FORMAT.into(), version: MANIFEST_VERSION, sequence: 0,
            store_id: store.store_id(), next_extent: 0, next_object: 0, next_root: 0,
            objects: BTreeMap::new(), roots: BTreeMap::new(),
        };
        store.publish_record(MANIFEST_ROOT, MANIFEST_MAGIC, 0, &serde_json::to_vec(&manifest)?)?;
        Ok(Self { store, manifest, pins: Arc::default(), poisoned: false, payload_reads: AtomicU64::new(0), lost_ack: None })
    }

    unsafe fn open(path: &Path) -> Result<Self> {
        ensure!(path.exists(), "rooted fixture is missing");
        let store = Store::open(path)?;
        let sequence = {
            let txn = sanakirja::Env::txn_begin(&store.owner.env)?;
            let db = txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(MANIFEST_ROOT)
                .ok_or_else(|| anyhow::anyhow!("missing rooted manifest schema"))?;
            let mut iter = sanakirja::btree::rev_iter(&txn, &db, None)?;
            let (&sequence, _) = iter.next().ok_or_else(|| anyhow::anyhow!("empty manifest index"))??;
            sequence
        };
        let bytes = store.read_record(MANIFEST_ROOT, MANIFEST_MAGIC, sequence)?
            .ok_or_else(|| anyhow::anyhow!("missing latest manifest"))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(manifest.format == MANIFEST_FORMAT && manifest.version == MANIFEST_VERSION,
            "unsupported rooted manifest schema");
        ensure!(manifest.sequence == sequence && manifest.store_id == store.store_id(),
            "manifest identity or sequence mismatch");
        let result = Self { store, manifest, pins: Arc::default(), poisoned: false, payload_reads: AtomicU64::new(0), lost_ack: None };
        result.validate_counters()?;
        result.mark()?;
        Ok(result)
    }

    fn usable(&self) -> Result<()> {
        ensure!(!self.poisoned, "controller is fail-stopped; drop and reopen before continuing");
        Ok(())
    }

    fn acknowledge(&self, site: CommitSite) -> Result<()> {
        ensure!(self.lost_ack != Some(site), "injected lost {site:?} commit acknowledgment");
        Ok(())
    }

    fn validate_counters(&self) -> Result<()> {
        for (&id, object) in &self.manifest.objects {
            ensure!(id < self.manifest.next_object, "object exceeds issued identity range");
            for descriptor in &object.extents {
                ensure!(descriptor.id < self.manifest.next_extent, "extent exceeds issued identity range");
            }
            ensure!(object.children.iter().all(|child| *child < id && self.manifest.objects.contains_key(child)), "object graph is missing a dependency or is not an immutable DAG");
        }
        for (&id, root) in &self.manifest.roots {
            ensure!(id < self.manifest.next_root, "root exceeds issued identity range");
            ensure!(self.manifest.objects.contains_key(&root.object), "root references missing object");
        }
        let txn = sanakirja::Env::txn_begin(&self.store.owner.env)?;
        if let Some(db) = txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(0) {
            for entry in sanakirja::btree::iter(&txn, &db, None)? {
                ensure!(*entry?.0 < self.manifest.next_extent, "unissued indexed extent");
            }
        }
        Ok(())
    }

    fn trace(&self, targets: impl IntoIterator<Item=u64>) -> Result<BTreeMap<u64, Descriptor>> {
        let mut pending: Vec<_> = targets.into_iter().collect();
        let mut visited = BTreeSet::new();
        let mut extents = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) { continue; }
            let object = self.manifest.objects.get(&id)
                .ok_or_else(|| anyhow::anyhow!("missing dependency object {id}"))?;
            ensure!(object.children.iter().all(|child| *child < id), "object graph is not an immutable DAG");
            pending.extend(object.children.iter().copied());
            for descriptor in &object.extents {
                if let Some(previous) = extents.insert(descriptor.id, descriptor.clone()) {
                    ensure!(previous == *descriptor, "conflicting dependency descriptors");
                }
            }
        }
        self.authenticate_metadata(extents.values())?;
        Ok(extents)
    }

    // Trace/authenticate metadata without creating a BytesSlice or decoding body
    // bytes. Header reads are still real storage I/O; this is not zero-I/O GC.
    fn authenticate_metadata<'a>(&self, descriptors: impl IntoIterator<Item=&'a Descriptor>) -> Result<()> {
        let txn = sanakirja::Env::txn_begin(&self.store.owner.env)?;
        let db = txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(0);
        for descriptor in descriptors {
            ensure!(descriptor.store_id == self.store.store_id(), "foreign descriptor");
            ensure!(descriptor.id < self.manifest.next_extent, "unissued descriptor");
            ensure!(descriptor.page > 0 && descriptor.page % PAGE as u64 == 0
                && descriptor.offset >= HEADER && (descriptor.offset - HEADER) % 8 == 0
                && descriptor.len > 0 && descriptor.offset.checked_add(descriptor.len).is_some_and(|end| end <= PAGE),
                "invalid descriptor range");
            let db = db.as_ref().ok_or_else(|| anyhow::anyhow!("missing payload index"))?;
            let (&id, &location) = sanakirja::btree::get(&txn, db, &descriptor.id, None)?
                .ok_or_else(|| anyhow::anyhow!("missing dependency extent"))?;
            ensure!(id == descriptor.id && location == descriptor.page + (descriptor.offset - HEADER) as u64,
                "missing or conflicting dependency extent");
            let page = unsafe { txn.load_page(descriptor.page)? };
            let raw = unsafe { std::slice::from_raw_parts(page.data, PAGE) };
            let slot = descriptor.offset - HEADER;
            ensure!(u64::from_le_bytes(raw[slot+8..slot+16].try_into()?) == MAGIC
                && u64::from_le_bytes(raw[slot+16..slot+24].try_into()?) == descriptor.len as u64
                && u64::from_le_bytes(raw[slot+24..slot+32].try_into()?) == descriptor.id,
                "dependency slot header mismatch");
        }
        Ok(())
    }

    fn mark(&self) -> Result<BTreeMap<u64, Descriptor>> {
        self.usable()?;
        let mut targets = self.manifest.roots.values().filter(|r| r.active).map(|r| r.object).collect::<Vec<_>>();
        targets.extend(self.pins.lock().unwrap().keys().copied());
        self.trace(targets)
    }

    fn persist(&mut self, mut next: Manifest) -> Result<()> {
        self.usable()?;
        next.sequence = self.manifest.sequence.checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("manifest sequence exhausted"))?;
        let bytes = serde_json::to_vec(&next)?;
        // Any error after mutation starts is conservatively ambiguous. No
        // further reads/writes through this facade; reopening reselects disk state.
        self.poisoned = true;
        self.store.publish_record(MANIFEST_ROOT, MANIFEST_MAGIC, next.sequence, &bytes)?;
        self.acknowledge(CommitSite::Manifest)?;
        self.manifest = next;
        self.poisoned = false;
        Ok(())
    }

    fn reserve_extent(&mut self) -> Result<ReservedExtent> {
        self.usable()?;
        let id = self.manifest.next_extent;
        let mut next = self.manifest.clone();
        next.next_extent = id.checked_add(1).ok_or_else(|| anyhow::anyhow!("extent IDs exhausted"))?;
        self.persist(next)?;
        Ok(ReservedExtent { id, store_id: self.store.store_id() })
    }

    fn allocate_reserved(&mut self, reserved: ReservedExtent, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> Result<(Descriptor, BytesSlice)> {
        self.usable()?;
        ensure!(reserved.store_id == self.store.store_id() && reserved.id < self.manifest.next_extent, "foreign or unissued reservation");
        ensure!(len > 0 && len <= PAGE - HEADER, "page-bounded extent required");
        self.poisoned = true;
        let result = self.store.insert_bound(reserved.id, len, generate)?;
        self.acknowledge(CommitSite::Payload)?;
        self.poisoned = false;
        Ok(result)
    }

    fn allocate(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> Result<(Descriptor, BytesSlice)> {
        self.usable()?;
        ensure!(len > 0 && len <= PAGE - HEADER, "page-bounded extent required");
        // Reserve durably FIRST. Failure/abandonment burns an ID, never reuses it.
        let reserved = self.reserve_extent()?;
        self.allocate_reserved(reserved, len, generate)
    }

    fn allocate_batch(&mut self, lengths: &[usize], generate: &mut dyn FnMut(usize, &mut [u8])) -> Result<Vec<(Descriptor, BytesSlice)>> {
        self.usable()?;
        if lengths.is_empty() { return Ok(Vec::new()); }
        ensure!(lengths.iter().all(|len| *len > 0 && *len <= PAGE - HEADER), "page-bounded extents required");
        let count = u64::try_from(lengths.len())?;
        let first = self.manifest.next_extent;
        let mut next = self.manifest.clone();
        next.next_extent = first.checked_add(count).ok_or_else(|| anyhow::anyhow!("extent IDs exhausted"))?;
        let entries = lengths.iter().enumerate().map(|(i, &len)| (first + i as u64, len)).collect::<Vec<_>>();
        self.persist(next)?;
        self.poisoned = true;
        let result = self.store.insert_batch_bound(&entries, generate)?;
        self.acknowledge(CommitSite::Payload)?;
        self.poisoned = false;
        Ok(result)
    }

    fn object(&mut self, children: Vec<u64>, extents: Vec<Descriptor>) -> Result<u64> {
        self.usable()?;
        self.trace(children.iter().copied())?;
        self.authenticate_metadata(extents.iter())?;
        let id = self.manifest.next_object;
        ensure!(children.iter().all(|child| *child < id), "dependency must precede object");
        let mut next = self.manifest.clone();
        next.next_object = id.checked_add(1).ok_or_else(|| anyhow::anyhow!("object IDs exhausted"))?;
        next.objects.insert(id, Object { children, extents });
        self.persist(next)?;
        Ok(id)
    }

    fn root(&mut self, kind: RootKind, object: u64) -> Result<u64> {
        self.usable()?;
        self.trace([object])?;
        let id = self.manifest.next_root;
        let mut next = self.manifest.clone();
        next.next_root = id.checked_add(1).ok_or_else(|| anyhow::anyhow!("root IDs exhausted"))?;
        next.roots.insert(id, Root { kind, object, active: true });
        self.persist(next)?;
        Ok(id)
    }

    fn retire(&mut self, id: u64) -> Result<()> {
        self.usable()?;
        let root = self.manifest.roots.get(&id).ok_or_else(|| anyhow::anyhow!("unknown root"))?;
        if !root.active { return Ok(()); }
        let mut next = self.manifest.clone();
        next.roots.get_mut(&id).unwrap().active = false;
        self.persist(next)
    }

    fn pin(&self, id: u64) -> Result<RootPin> {
        self.usable()?;
        let root = self.manifest.roots.get(&id).filter(|r| r.active)
            .ok_or_else(|| anyhow::anyhow!("inactive or unknown root"))?;
        self.trace([root.object])?;
        {
            let mut pins = self.pins.lock().unwrap();
            let count = pins.entry(root.object).or_default();
            *count = count.checked_add(1).ok_or_else(|| anyhow::anyhow!("root pin count exhausted"))?;
        }
        Ok(RootPin(Arc::new(RootPinInner { object: root.object, pins: self.pins.clone(), owner: self.store.owner.clone() })))
    }

    fn read(&self, pin: &RootPin, id: u64) -> Result<BytesSlice> {
        self.usable()?;
        ensure!(Arc::ptr_eq(&pin.0.owner, &self.store.owner), "foreign root pin");
        let traced = self.trace([pin.0.object])?;
        let descriptor = traced.get(&id).ok_or_else(|| anyhow::anyhow!("extent is outside pinned root"))?;
        self.payload_reads.fetch_add(1, Ordering::SeqCst);
        self.store.bind(descriptor)
    }

    fn sweep(&mut self) -> Result<Sweep> {
        let retained = self.mark()?.keys().copied().collect();
        self.poisoned = true;
        let result = sweep_with_manifest(&mut self.store, &retained, Some(self.manifest.sequence))?;
        if !result.pages.is_empty() { self.acknowledge(CommitSite::Sweep)?; }
        self.poisoned = false;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::reclamation_probe::reuse_pages;

    fn leaf(store: &mut RootedStore, fill: u8) -> (u64, Descriptor) {
        let (descriptor, bytes) = store.allocate(256, &mut |out| out.fill(fill)).unwrap();
        drop(bytes);
        let object = store.object(Vec::new(), vec![descriptor.clone()]).unwrap();
        (object, descriptor)
    }

    fn child_phase(path: &Path, phase: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::rooted_reclamation::tests::fresh_process_phase", "--nocapture"])
            .env("LORO_ROOTED_FIXTURE_PATH", path)
            .env("LORO_ROOTED_FIXTURE_PHASE", phase)
            .output().unwrap();
        assert!(output.status.success(), "phase {phase} failed:\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }

    #[test]
    fn shared_roots_and_lazy_history_survive_fresh_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        for phase in ["create", "retire-one", "release-all", "reopen-empty"] {
            child_phase(&path, phase);
        }
    }

    #[test]
    fn fresh_process_phase() {
        let Ok(phase) = std::env::var("LORO_ROOTED_FIXTURE_PHASE") else { return; };
        let path = std::path::PathBuf::from(std::env::var_os("LORO_ROOTED_FIXTURE_PATH").unwrap());
        match phase.as_str() {
            "create" => {
                let mut store = unsafe { RootedStore::create(&path).unwrap() };
                let (shared, _) = leaf(&mut store, b's'); // extent/object 0
                let (left, _) = leaf(&mut store, b'l');   // extent/object 1
                let (right, _) = leaf(&mut store, b'r');  // extent/object 2
                let (orphan, bytes) = store.allocate(256, &mut |out| out.fill(b'x')).unwrap();
                assert_eq!(orphan.id, 3);
                drop(bytes);
                let left_root = store.object(vec![shared, left], vec![]).unwrap();
                let right_root = store.object(vec![shared, right], vec![]).unwrap();
                assert_eq!(store.root(RootKind::History, left_root).unwrap(), 0);
                assert_eq!(store.root(RootKind::Current, right_root).unwrap(), 1);
                assert_eq!(store.payload_reads.load(Ordering::SeqCst), 0);
                assert!(store.store.live_segments.lock().unwrap().is_empty());
            }
            "retire-one" => {
                let mut store = unsafe { RootedStore::open(&path).unwrap() };
                assert!(store.store.live_segments.lock().unwrap().is_empty());
                let initial = store.sweep().unwrap();
                assert_eq!(initial.retired_ids, BTreeSet::from([3]));
                reuse_pages(&mut store.store, &initial.pages);
                store.retire(0).unwrap();
                let left = store.sweep().unwrap();
                assert_eq!(left.retired_ids, BTreeSet::from([1]));
                reuse_pages(&mut store.store, &left.pages);
                assert_eq!(store.payload_reads.load(Ordering::SeqCst), 0);
                assert_eq!(store.store.metrics().resolutions, 0);
                // Retained old object metadata must not resurrect a swept dependency.
                let before = std::fs::read(&path).unwrap();
                assert!(store.root(RootKind::History, 3).is_err());
                assert_eq!(before, std::fs::read(&path).unwrap());
                let pin = store.pin(1).unwrap();
                assert_eq!(&*store.read(&pin, 0).unwrap(), vec![b's'; 256]);
                assert_eq!(&*store.read(&pin, 2).unwrap(), vec![b'r'; 256]);
                println!("MEASUREMENT {{\"scenario\":\"durable_shared_roots\",\"fresh_process\":true,\"orphan_retired\":1,\"one_root_unique_retired\":1,\"shared_retained\":true,\"payload_bindings_before_first_read\":0}}");
            }
            "release-all" => {
                let mut store = unsafe { RootedStore::open(&path).unwrap() };
                assert!(!store.manifest.roots[&0].active);
                assert!(store.manifest.roots[&1].active);
                let pin = store.pin(1).unwrap();
                let clone = pin.clone();
                store.retire(1).unwrap();
                drop(pin);
                assert!(store.sweep().unwrap().pages.is_empty());
                assert_eq!(store.payload_reads.load(Ordering::SeqCst), 0);
                let bytes = store.read(&clone, 0).unwrap();
                let address = bytes.as_ptr();
                drop(clone);
                let right = store.sweep().unwrap();
                assert_eq!(right.retired_ids, BTreeSet::from([2]));
                reuse_pages(&mut store.store, &right.pages);
                assert_eq!(&*bytes, vec![b's'; 256]);
                assert_eq!(bytes.as_ptr(), address);
                drop(bytes);
                let shared = store.sweep().unwrap();
                assert_eq!(shared.retired_ids, BTreeSet::from([0]));
                reuse_pages(&mut store.store, &shared.pages);
                println!("MEASUREMENT {{\"scenario\":\"retired_durable_root_with_lazy_pin\",\"reclaimed_while_unread_pin_alive\":0,\"after_pin_drop_with_byte_lease\":1,\"after_last_byte_lease\":1}}");
            }
            "reopen-empty" => {
                let mut store = unsafe { RootedStore::open(&path).unwrap() };
                assert!(store.manifest.roots.values().all(|root| !root.active));
                assert!(store.sweep().unwrap().pages.is_empty());
                assert!(store.root(RootKind::EditingBranch, 4).is_err());
                let (fresh, bytes) = store.allocate(256, &mut |out| out.fill(b'n')).unwrap();
                assert_eq!(fresh.id, 4);
                let object = store.object(vec![], vec![fresh]).unwrap();
                assert_eq!(object, 5);
                let root = store.root(RootKind::EditingBranch, object).unwrap();
                assert_eq!(root, 2);
                drop(bytes);
                store.retire(root).unwrap();
                assert_eq!(store.sweep().unwrap().retired_ids, BTreeSet::from([4]));
                println!("MEASUREMENT {{\"scenario\":\"retired_roots_reopen\",\"tombstones_preserved\":true,\"dead_metadata_admitted\":true,\"collected_objects_cannot_be_republished\":true,\"next_extent_id\":4}}");
            }
            _ => panic!("unknown child phase"),
        }
    }

    #[test]
    fn durable_nonreused_ids_reject_stale_descriptor_after_actual_page_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let mut seen = BTreeMap::<(u64, usize, usize), Descriptor>::new();
        for generation in 0..64u8 {
            // Reserve before retiring the preceding payload so the manifest
            // write cannot consume that newly freed page before payload allocation.
            let reserved = store.reserve_extent().unwrap();
            store.sweep().unwrap();
            let (descriptor, bytes) = store.allocate_reserved(reserved, 256, &mut |out| out.fill(b'!' + generation)).unwrap();
            let location = (descriptor.page, descriptor.offset, descriptor.len);
            if let Some(stale) = seen.get(&location).cloned() {
                assert_ne!(descriptor.id, stale.id);
                assert!(store.store.bind(&stale).is_err());
                assert_eq!(&*store.store.bind(&descriptor).unwrap(), &*bytes);
                drop(bytes);
                drop(store);
                let reopened = unsafe { RootedStore::open(&path).unwrap() };
                assert!(reopened.store.bind(&stale).is_err());
                assert_eq!(&*reopened.store.bind(&descriptor).unwrap(), vec![b'!' + generation; 256]);
                println!("MEASUREMENT {{\"scenario\":\"durable_identity_prevents_aba\",\"physical_location_reused\":true,\"generation\":{generation},\"stale_descriptor_rejected_before_and_after_reopen\":true}}");
                return;
            }
            seen.insert(location, descriptor);
            drop(bytes);
        }
        panic!("pinned allocator did not reuse a payload location within 64 allocations");
    }

    #[test]
    fn abandoned_failed_and_panicking_allocations_burn_durable_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let reserved = store.reserve_extent().unwrap();
        assert_eq!(reserved.id, 0);
        drop(reserved);
        assert!(store.allocate(1, &mut |out| out[0] = 255).is_err()); // id 1
        assert!(store.poisoned);
        assert!(store.sweep().is_err());
        assert!(store.reserve_extent().is_err());
        drop(store);
        let mut store = unsafe { RootedStore::open(&path).unwrap() };
        assert_eq!(store.manifest.next_extent, 2);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.allocate(1, &mut |_| panic!("abandoned generated allocation"));
        }));
        assert!(result.is_err());
        assert!(store.poisoned);
        drop(store);
        let mut store = unsafe { RootedStore::open(&path).unwrap() };
        let (descriptor, bytes) = store.allocate(1, &mut |out| out[0] = b'z').unwrap();
        assert_eq!(descriptor.id, 3);
        assert_eq!(&*bytes, b"z");
        println!("MEASUREMENT {{\"scenario\":\"burned_identity_reservations\",\"abandoned_id\":0,\"invalid_utf8_id\":1,\"panicked_id\":2,\"next_successful_id\":3,\"fail_stop_enforced\":true}}");
    }

    #[test]
    fn lazy_pin_clones_keep_exclusion_after_controller_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let (object, _) = leaf(&mut store, b'x');
        let root = store.root(RootKind::History, object).unwrap();
        let pin = store.pin(root).unwrap();
        let clone = pin.clone();
        store.retire(root).unwrap();
        drop(pin);
        drop(store);
        assert!(unsafe { RootedStore::open(&path) }.is_err());
        drop(clone);
        let mut reopened = unsafe { RootedStore::open(&path).unwrap() };
        assert_eq!(reopened.sweep().unwrap().retired_ids, BTreeSet::from([0]));
    }

    #[test]
    fn invalid_latest_schema_or_graph_refuses_without_fallback_or_writes() {
        for invalid in ["schema", "child", "counter", "descriptor", "json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            let mut store = unsafe { RootedStore::create(&path).unwrap() };
            let (leaf, _) = leaf(&mut store, b'x');
            let parent = store.object(vec![leaf], vec![]).unwrap();
            store.root(RootKind::Current, parent).unwrap();
            let mut invalid_manifest = store.manifest.clone();
            invalid_manifest.sequence += 1;
            match invalid {
                "schema" => invalid_manifest.version += 1,
                "child" => { invalid_manifest.objects.remove(&leaf); },
                "counter" => invalid_manifest.next_extent = 0,
                "descriptor" => invalid_manifest.objects.get_mut(&leaf).unwrap().extents[0].offset += 8,
                "json" => {},
                _ => unreachable!(),
            }
            let bytes = if invalid == "json" { b"{broken".to_vec() } else { serde_json::to_vec(&invalid_manifest).unwrap() };
            store.store.publish_record(MANIFEST_ROOT, MANIFEST_MAGIC, invalid_manifest.sequence, &bytes).unwrap();
            drop(store);
            let before = std::fs::read(&path).unwrap();
            assert!(unsafe { RootedStore::open(&path) }.is_err());
            assert_eq!(before, std::fs::read(&path).unwrap());
        }
    }

    #[test]
    fn legacy_index_without_manifest_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy");
        let mut legacy = unsafe { Store::open(&path).unwrap() };
        drop(legacy.insert(17, 1, &mut |out| out[0] = b'x').unwrap());
        drop(legacy);
        let before = std::fs::read(&path).unwrap();
        assert!(unsafe { RootedStore::open(&path) }.is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }

    #[test]
    fn counter_exhaustion_and_foreign_pin_are_read_only_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let (object, _) = leaf(&mut store, b'x');
        let root = store.root(RootKind::Current, object).unwrap();
        let pin = store.pin(root).unwrap();
        let other_path = dir.path().join("other");
        let mut other = unsafe { RootedStore::create(&other_path).unwrap() };
        assert!(other.read(&pin, 0).is_err());
        let reservation = store.reserve_extent().unwrap();
        let other_before = std::fs::read(&other_path).unwrap();
        assert!(other.allocate_reserved(reservation, 1, &mut |out| out[0] = b'z').is_err());
        assert_eq!(other_before, std::fs::read(&other_path).unwrap());
        assert!(!other.poisoned);
        let mut exhausted = store.manifest.clone();
        exhausted.next_extent = u64::MAX;
        store.persist(exhausted).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(store.reserve_extent().is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert!(!store.poisoned);
    }

    #[test]
    fn traced_roots_and_byte_leases_respect_packed_page_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packed");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let batch = store.allocate_batch(&[256; 28], &mut |i, out| out.fill(b'a' + (i % 26) as u8)).unwrap();
        let descriptors = batch.iter().map(|(d, _)| d.clone()).collect::<Vec<_>>();
        assert_eq!(descriptors.iter().map(|d| d.page).collect::<BTreeSet<_>>().len(), 2);
        drop(batch);
        let left = store.object(vec![], vec![descriptors[0].clone()]).unwrap();
        let right = store.object(vec![], vec![descriptors[14].clone()]).unwrap();
        let left_root = store.root(RootKind::History, left).unwrap();
        let right_root = store.root(RootKind::Current, right).unwrap();
        assert!(store.sweep().unwrap().pages.is_empty());
        store.retire(left_root).unwrap();
        let first = store.sweep().unwrap();
        assert_eq!(first.pages, BTreeSet::from([descriptors[0].page]));
        assert_eq!(first.retired_ids, (0..14).collect());
        reuse_pages(&mut store.store, &first.pages);
        let pin = store.pin(right_root).unwrap();
        let bytes = store.read(&pin, 14).unwrap();
        store.retire(right_root).unwrap();
        drop(pin);
        assert!(store.sweep().unwrap().pages.is_empty());
        assert_eq!(&*bytes, vec![b'o'; 256]);
        drop(bytes);
        let final_page = store.sweep().unwrap();
        assert_eq!(final_page.pages, BTreeSet::from([descriptors[14].page]));
        assert_eq!(final_page.retired_ids, (14..28).collect());
        reuse_pages(&mut store.store, &final_page.pages);
        println!("MEASUREMENT {{\"scenario\":\"rooted_packed_pages\",\"payload_pages\":2,\"with_both_roots\":0,\"after_one_root_retired\":1,\"with_final_byte_lease\":0,\"after_lease_drop\":1}}");
    }

    #[test]
    fn untraced_or_stale_manifest_sweep_is_refused_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let (object, _) = leaf(&mut store, b'x');
        store.root(RootKind::Current, object).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(sweep_with_manifest(&mut store.store, &BTreeSet::new(), None).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert!(sweep_with_manifest(&mut store.store, &BTreeSet::new(), Some(store.manifest.sequence - 1)).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert!(store.sweep().unwrap().pages.is_empty());
    }


    #[test]
    fn lost_success_after_actual_commits_fail_stops_and_reopens_disk_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { RootedStore::create(&path).unwrap() };
        let (object, _) = leaf(&mut store, b'x');
        let root = store.root(RootKind::History, object).unwrap();
        let old_sequence = store.manifest.sequence;
        store.lost_ack = Some(CommitSite::Manifest);
        assert!(store.retire(root).is_err());
        assert!(store.poisoned);
        assert!(store.manifest.roots[&root].active); // stale local state, never reused
        let before = std::fs::read(&path).unwrap();
        assert!(store.retire(root).is_err());
        assert!(store.pin(root).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        drop(store);
        let mut store = unsafe { RootedStore::open(&path).unwrap() };
        assert!(!store.manifest.roots[&root].active);
        assert_eq!(store.manifest.sequence, old_sequence + 1);
        assert_eq!(store.sweep().unwrap().retired_ids, BTreeSet::from([0]));

        store.lost_ack = Some(CommitSite::Payload);
        assert!(store.allocate(256, &mut |out| out.fill(b'y')).is_err());
        assert!(store.poisoned);
        drop(store);
        let mut store = unsafe { RootedStore::open(&path).unwrap() };
        assert_eq!(store.manifest.next_extent, 2);
        assert_eq!(&*store.store.get(1).unwrap(), vec![b'y'; 256]);
        assert_eq!(store.sweep().unwrap().retired_ids, BTreeSet::from([1]));

        let (descriptor, bytes) = store.allocate(256, &mut |out| out.fill(b'z')).unwrap();
        assert_eq!(descriptor.id, 2);
        drop(bytes);
        store.lost_ack = Some(CommitSite::Sweep);
        assert!(store.sweep().is_err());
        assert!(store.poisoned);
        let before = std::fs::read(&path).unwrap();
        assert!(store.sweep().is_err());
        assert!(store.reserve_extent().is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        drop(store);
        let mut store = unsafe { RootedStore::open(&path).unwrap() };
        assert!(store.store.bind(&descriptor).is_err());
        assert!(store.sweep().unwrap().pages.is_empty());
        let (new, _) = store.allocate(256, &mut |out| out.fill(b'!')).unwrap();
        assert_eq!(new.id, 3);
        println!("MEASUREMENT {{\"scenario\":\"lost_commit_acknowledgments\",\"actual_successful_commits\":3,\"fail_stop_and_reopen_verified\":true,\"engine_io_failure_injected\":false,\"crash_test\":false}}");
    }

}
