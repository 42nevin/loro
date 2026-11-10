//! TEST-ONLY whole-page retirement for a closed-world, unpublished fixture.
//! This is not an application GC or a supported Store API. Public/derived
//! records are refused: their opaque metadata has no root enumerator here.
use super::*;
use loro::{LoroDoc, TextExtentAllocator, TextSlice};
use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Default)]
pub(super) struct Sweep {
    pub(super) pages: BTreeSet<u64>,
    pub(super) retired_ids: BTreeSet<u64>,
}

fn sweep_unpublished(store: &mut Store, retained: &BTreeSet<u64>) -> Result<Sweep> {
    sweep_with_manifest(store, retained, None)
}

// Only the sibling test-only rooted controller supplies a sequence, after
// validating and tracing its complete versioned manifest under an exclusive
// Store borrow. The unpublished entry point refuses that extra root entirely.
pub(super) fn sweep_with_manifest(
    store: &mut Store,
    retained: &BTreeSet<u64>,
    manifest_sequence: Option<u64>,
) -> Result<Sweep> {
    // The exclusive Store borrow excludes concurrent bind/insert. Outstanding
    // BytesSlice clones can only keep an existing segment alive or release it.
    // Weak entries are never upgraded while holding this mutex (Drop relocks).
    let leased: BTreeSet<u64> = store.live_segments.lock().unwrap().iter()
        .filter(|(_, weak)| weak.strong_count() != 0).map(|(&id, _)| id).collect();
    let mut by_page: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut all_ids = BTreeSet::new();
    {
        // Validate every refusal before mut_txn_begin, which copies root bytes.
        let txn = sanakirja::Env::txn_begin(&store.owner.env)?;
        for root in [1, 3, 4] {
            ensure!(txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(root).is_none(),
                "probe refuses opaque published or derived records");
        }
        let manifest_db = txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(5);
        match (manifest_sequence, manifest_db) {
            (None, None) => {},
            (Some(expected), Some(db)) => {
                let mut iter = sanakirja::btree::rev_iter(&txn, &db, None)?;
                let (&actual, _) = iter.next().ok_or_else(|| anyhow::anyhow!("empty manifest index"))??;
                ensure!(actual == expected, "manifest changed before sweep");
            },
            _ => anyhow::bail!("probe refuses untraced manifest root"),
        }
        let Some(db) = txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(0) else {
            ensure!(retained.is_empty(), "unknown retained extent");
            return Ok(Sweep::default());
        };
        for entry in sanakirja::btree::iter(&txn, &db, None)? {
            let (&id, &location) = entry?;
            let page = location & !((PAGE as u64) - 1);
            let slot = usize::try_from(location - page)?;
            ensure!(page != 0 && slot % 8 == 0 && slot + HEADER <= PAGE, "invalid indexed slot");
            let loaded = unsafe { txn.load_page(page)? };
            let raw = unsafe { std::slice::from_raw_parts(loaded.data, PAGE) };
            ensure!(u64::from_le_bytes(raw[slot+8..slot+16].try_into()?) == MAGIC, "invalid slot magic");
            ensure!(u64::from_le_bytes(raw[slot+24..slot+32].try_into()?) == id, "invalid slot identity");
            let len = usize::try_from(u64::from_le_bytes(raw[slot+16..slot+24].try_into()?))?;
            ensure!(len > 0 && len <= PAGE-slot-HEADER, "invalid slot length");
            by_page.entry(page).or_default().push(id);
            all_ids.insert(id);
        }
    }
    ensure!(retained.is_subset(&all_ids), "unknown retained extent");
    ensure!(leased.is_subset(&all_ids), "lease missing from durable index");
    let mut swept = Sweep::default();
    for (page, ids) in by_page {
        if ids.iter().all(|id| !retained.contains(id) && !leased.contains(id)) {
            swept.pages.insert(page);
            swept.retired_ids.extend(ids);
        }
    }
    if swept.pages.is_empty() { return Ok(swept); }
    let mut txn = sanakirja::Env::mut_txn_begin(&store.owner.env)?;
    let mut db = txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(0).unwrap();
    for id in &swept.retired_ids {
        ensure!(sanakirja::btree::del(&mut txn, &mut db, id, None)?, "extent disappeared");
    }
    for &page in &swept.pages {
        // SAFETY: fixture-owned, committed raw payload page. Every indexed
        // extent on it is absent from explicit retained roots and all leases;
        // no opaque public/derived records exist. Decrement exactly once per
        // page, never once per slot. u64 index values have no automatic RC.
        ensure!(unsafe { txn.decr_rc(page)? } == 0, "unexpected payload page RC");
    }
    txn.set_root(0, db.db.into());
    txn.commit()?;
    Ok(swept)
}

struct BindFixture {
    id: u64,
    bytes: BytesSlice,
}
impl TextExtentAllocator for BindFixture {
    fn allocate(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> std::io::Result<BytesSlice> {
        let mut check = vec![0; len];
        generate(&mut check);
        assert_eq!(check.as_slice(), &*self.bytes);
        Ok(self.bytes.clone())
    }
    fn allocate_text(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> std::io::Result<TextSlice> {
        let bytes = self.allocate(len, generate)?;
        TextSlice::direct_extent(self.id, bytes)
    }
}

fn body(id: u64) -> Vec<u8> {
    // Five identical logical survivors across both layouts; unique filler bytes.
    vec![b'a' + (id % 26) as u8; 256]
}
fn fixture(path: &Path, scattered: bool) -> (Store, Vec<Descriptor>) {
    let mut store = unsafe { Store::open(path).unwrap() };
    let mut order = (0..70u64).collect::<Vec<_>>();
    if scattered {
        // 14 slots fit per page: 14 * (32 + 256) = 4032 bytes.
        for id in 1..5 { order.swap(id, id * 14); }
    }
    let entries = order.iter().map(|&id| (id, 256)).collect::<Vec<_>>();
    let batch = store.insert_batch_bound(&entries, &mut |i, dst| dst.copy_from_slice(&body(order[i]))).unwrap();
    let mut descriptors = batch.iter().map(|(d, _)| d.clone()).collect::<Vec<_>>();
    descriptors.sort_by_key(|d| d.id);
    assert_eq!(descriptors.iter().map(|d| d.page).collect::<BTreeSet<_>>().len(), 5);
    assert_eq!(descriptors[..5].iter().map(|d| d.page).collect::<BTreeSet<_>>().len(), if scattered {5} else {1});
    drop(batch);
    assert!(store.live_segments.lock().unwrap().is_empty());
    (store, descriptors)
}
fn doc_from(store: &Store, ids: std::ops::Range<u64>) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    for id in ids {
        let mut binding = BindFixture { id, bytes: store.get(id).unwrap() };
        let text = body(id);
        doc.get_text("text").insert_unicode_with_allocator(doc.get_text("text").len_unicode(), std::str::from_utf8(&text).unwrap(), &mut binding).unwrap();
        doc.commit();
    }
    doc
}
fn handles(doc: &LoroDoc) -> BTreeMap<u64, usize> {
    doc.inspect_text_op_handles().iter().map(|slice| {
        let info = slice.backing_info();
        assert_eq!(info.kind, "direct_extent");
        let id = info.extent.unwrap();
        let bytes = slice.direct_bytes().unwrap();
        assert_eq!(bytes, &body(id));
        (id, bytes.as_ptr() as usize)
    }).collect()
}
fn assert_retained(store: &Store, descriptors: &[Descriptor], addresses: &BTreeMap<u64,usize>) {
    for (&id, &ptr) in addresses {
        let bytes = store.bind(&descriptors[id as usize]).unwrap();
        assert_eq!(bytes.as_ptr() as usize, ptr);
        assert_eq!(&*bytes, &body(id));
    }
}
pub(super) fn reuse_pages(store: &mut Store, expected: &BTreeSet<u64>) -> BTreeSet<u64> {
    if expected.is_empty() { return BTreeSet::new(); }
    let mut txn = sanakirja::Env::mut_txn_begin(&store.owner.env).unwrap();
    let mut reused = BTreeSet::new();
    for _ in 0..expected.len() * 8 + 32 {
        // No prior read transaction remains. Only pages the engine allocates
        // may be overwritten; retained payload pages must never appear here.
        let page = unsafe { txn.alloc_page().unwrap() };
        unsafe { std::slice::from_raw_parts_mut(page.0.data, PAGE) }.fill(0xee);
        if expected.contains(&page.0.offset) { reused.insert(page.0.offset); }
        if &reused == expected { break; }
    }
    // Abort reservations after proving allocator reuse. Retired original bytes
    // may be overwritten, but no new persistent object is published by this probe.
    drop(txn);
    assert_eq!(&reused, expected);
    reused
}

#[test]
fn five_extents_packed_or_scattered_with_old_branch_and_history() {
    for scattered in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let (mut store, descriptors) = fixture(&path, scattered);
        let current = doc_from(&store, 0..5);
        let cut = current.oplog_frontiers();
        let history = current.fork_cow().unwrap();
        history.checkout(&cut).unwrap();
        let old_branch = current.fork_cow().unwrap();
        old_branch.set_peer_id(9).unwrap();
        old_branch.get_text("text").delete(0, 1).unwrap();
        old_branch.commit();
        let addresses = handles(&current);
        assert_eq!(addresses.len(), 5);
        assert_eq!(handles(&history), addresses);
        assert_eq!(handles(&old_branch), addresses);
        let root_ids = addresses.keys().copied().collect();
        let before = std::fs::metadata(&path).unwrap();
        let sweep = sweep_unpublished(&mut store, &root_ids).unwrap();
        let expected = if scattered {0} else {4};
        assert_eq!(sweep.pages.len(), expected);
        assert_eq!(sweep.retired_ids.len(), expected * 14);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        // Allocation blocks are measured, not assumed: metadata CoW can grow
        // them even when payload pages become reusable. No hole punching.
        assert_eq!(reuse_pages(&mut store, &sweep.pages).len(), expected);
        assert_retained(&store, &descriptors, &addresses);
        assert_eq!(current.get_text("text").len_unicode(), 1280);
        assert_eq!(history.get_text("text").len_unicode(), 1280);
        assert_eq!(old_branch.get_text("text").len_unicode(), 1279);
        // Continue editing the old branch after actual free/reuse.
        old_branch.get_text("text").delete(0, 1).unwrap();
        old_branch.commit();
        assert_eq!(old_branch.get_text("text").len_unicode(), 1278);
        assert_eq!(history.get_text("text").len_unicode(), 1280);
        println!("MEASUREMENT {{\"scenario\":\"five_live\",\"layout\":\"{}\",\"payload_pages\":5,\"retained_extents\":5,\"baseline_reclaimed\":0,\"probe_reclaimed\":{},\"allocator_reused\":{},\"retired_ids\":{},\"file_before\":{},\"file_after_sweep\":{},\"allocated_before\":{},\"allocated_after_sweep\":{},\"live_relocation_bytes\":0,\"old_branch_edit\":true,\"historical_view\":true}}", if scattered {"scattered"} else {"concentrated"}, expected, expected, sweep.retired_ids.len(), before.len(), after.len(), before.blocks()*512, after.blocks()*512);
        drop(current); drop(history); drop(old_branch); drop(store);
        let reopened = unsafe { Store::open(&path).unwrap() };
        for id in 0..70u64 {
            if sweep.retired_ids.contains(&id) { assert!(reopened.bind(&descriptors[id as usize]).is_err()); }
            else { assert_eq!(&*reopened.bind(&descriptors[id as usize]).unwrap(), &body(id)); }
        }
    }
}

#[test]
fn long_lived_lease_delays_whole_page_not_other_pages() {
    let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("data");
    let (mut store, descriptors) = fixture(&path, false);
    let doc = doc_from(&store, 0..5);
    let root_ids = (0..5).collect();
    let lease = store.get(69).unwrap();
    let address = lease.as_ptr();
    let first = sweep_unpublished(&mut store, &root_ids).unwrap();
    assert_eq!(first.pages.len(), 3);
    reuse_pages(&mut store, &first.pages);
    assert_eq!(&*lease, &body(69)); assert_eq!(lease.as_ptr(), address);
    assert!(!first.pages.contains(&descriptors[69].page));
    drop(lease);
    let second = sweep_unpublished(&mut store, &root_ids).unwrap();
    assert_eq!(second.pages, BTreeSet::from([descriptors[69].page]));
    reuse_pages(&mut store, &second.pages);
    assert_eq!(doc.get_text("text").len_unicode(), 1280);
    println!("MEASUREMENT {{\"scenario\":\"long_lease\",\"first_sweep\":3,\"after_lease_drop\":1,\"live_relocation_bytes\":0}}");
}

#[test]
fn visible_deletion_does_not_retire_native_history() {
    for scattered in [false, true] {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("data");
        let (mut store, _) = fixture(&path, scattered);
        let doc = doc_from(&store, 0..70);
        let cut = doc.oplog_frontiers();
        let history = doc.fork_cow().unwrap(); history.checkout(&cut).unwrap();
        let old_branch = doc.fork_cow().unwrap();
        doc.get_text("text").delete(1280, 65*256).unwrap(); doc.commit();
        assert_eq!(doc.get_text("text").len_unicode(), 1280);
        assert_eq!(handles(&doc).len(), 70);
        assert_eq!(sweep_unpublished(&mut store, &BTreeSet::new()).unwrap().pages.len(), 0);
        old_branch.get_text("text").delete(0, 1).unwrap(); old_branch.commit();
        assert_eq!(old_branch.get_text("text").len_unicode(), 70*256-1);
        assert_eq!(history.get_text("text").len_unicode(), 70*256);
        drop(old_branch); drop(history);
        assert_eq!(sweep_unpublished(&mut store, &BTreeSet::new()).unwrap().pages.len(), 0);
        doc.checkout(&cut).unwrap(); assert_eq!(doc.get_text("text").len_unicode(), 70*256);
        doc.checkout_to_latest(); assert_eq!(doc.get_text("text").len_unicode(), 1280);
        drop(doc);
        let swept = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
        assert_eq!(swept.pages.len(), 5); reuse_pages(&mut store, &swept.pages);
        println!("MEASUREMENT {{\"scenario\":\"deleted_visible_text\",\"layout\":\"{}\",\"visible_extents\":5,\"history_retained_extents\":70,\"reclaimed_with_branch_and_view\":0,\"reclaimed_after_branch_and_view_drop\":0,\"reclaimed_after_all_docs_drop_unpublished_only\":5}}", if scattered {"scattered"} else {"concentrated"});
    }
}

#[test]
fn durable_roots_without_live_lease_and_public_metadata_fail_closed() {
    let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("data");
    let (mut store, descriptors) = fixture(&path, false);
    let roots = BTreeSet::from([0,69]);
    assert!(store.live_segments.lock().unwrap().is_empty());
    let swept = sweep_unpublished(&mut store, &roots).unwrap();
    assert_eq!(swept.pages.len(), 3);
    for id in roots { assert_eq!(&*store.bind(&descriptors[id as usize]).unwrap(), &body(id)); }
    let before = std::fs::read(&path).unwrap();
    assert!(sweep_unpublished(&mut store, &BTreeSet::from([999])).is_err());
    assert_eq!(before, std::fs::read(&path).unwrap());
    store.publish(0, b"opaque application metadata cannot be traced", &[]).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(sweep_unpublished(&mut store, &BTreeSet::new()).is_err());
    assert_eq!(before, std::fs::read(&path).unwrap());
}

#[test]
fn old_editing_branch_alone_pins_history_then_one_cloned_handle_pins_one_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let (mut store, descriptors) = fixture(&path, false);
    let current = doc_from(&store, 0..70);
    let cut = current.oplog_frontiers();
    let history = current.fork_cow().unwrap();
    history.checkout(&cut).unwrap();
    let branch = current.fork_cow().unwrap();
    branch.set_peer_id(19).unwrap();
    current.get_text("text").delete(1280, 65 * 256).unwrap();
    current.commit();
    drop(current);
    assert!(sweep_unpublished(&mut store, &BTreeSet::new()).unwrap().pages.is_empty());
    assert_eq!(history.get_text("text").len_unicode(), 70 * 256);
    drop(history);
    // The editable old branch is now the only document. Current visibility and
    // the lifetime of the document from which it forked do not determine liveness.
    assert!(sweep_unpublished(&mut store, &BTreeSet::new()).unwrap().pages.is_empty());
    branch.get_text("text").delete(0, 1).unwrap();
    branch.commit();
    assert_eq!(branch.get_text("text").len_unicode(), 70 * 256 - 1);
    let lease = store.get(69).unwrap();
    let address = lease.as_ptr() as usize;
    let clone = lease.clone();
    let (release, wait) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        wait.recv().unwrap();
        assert_eq!(&*clone, &body(69));
        assert_eq!(clone.as_ptr() as usize, address);
    });
    drop(lease);
    drop(branch);
    let first = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
    assert_eq!(first.pages.len(), 4);
    assert!(!first.pages.contains(&descriptors[69].page));
    reuse_pages(&mut store, &first.pages);
    assert!(sweep_unpublished(&mut store, &BTreeSet::new()).unwrap().pages.is_empty());
    release.send(()).unwrap();
    worker.join().unwrap();
    let last = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
    assert_eq!(last.pages, BTreeSet::from([descriptors[69].page]));
    reuse_pages(&mut store, &last.pages);
    println!("MEASUREMENT {{\"scenario\":\"branch_then_cross_thread_handle\",\"reclaimed_with_old_branch_only\":0,\"after_branch_drop\":4,\"after_final_clone_drop\":1,\"live_relocation_bytes\":0}}");
}

#[test]
fn abandoning_private_branch_releases_only_its_unrooted_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let (mut store, descriptors) = fixture(&path, false);
    let current = doc_from(&store, 0..5);
    let addresses = handles(&current);
    let branch = current.fork_cow().unwrap();
    branch.set_peer_id(29).unwrap();
    let (private, bytes) = store.insert_bound(70, 256, &mut |out| out.copy_from_slice(&body(70))).unwrap();
    let mut allocator = BindFixture { id: 70, bytes };
    branch.get_text("text").insert_unicode_with_allocator(1280, std::str::from_utf8(&body(70)).unwrap(), &mut allocator).unwrap();
    branch.commit();
    drop(allocator);
    assert!(descriptors.iter().all(|d| d.page != private.page));
    let first = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
    assert_eq!(first.pages.len(), 4);
    assert!(!first.pages.contains(&private.page));
    reuse_pages(&mut store, &first.pages);
    assert_eq!(branch.get_text("text").len_unicode(), 1536);
    assert_eq!(current.get_text("text").len_unicode(), 1280);
    drop(branch);
    let abandoned = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
    assert_eq!(abandoned.pages, BTreeSet::from([private.page]));
    assert_eq!(abandoned.retired_ids, BTreeSet::from([70]));
    reuse_pages(&mut store, &abandoned.pages);
    assert_retained(&store, &descriptors, &addresses);
    assert_eq!(current.get_text("text").len_unicode(), 1280);
    println!("MEASUREMENT {{\"scenario\":\"abandoned_private_branch\",\"orphan_pages_first\":4,\"private_branch_page_after_drop\":1,\"current_page_retained\":1,\"live_relocation_bytes\":0}}");
}

#[test]
fn restored_explicit_roots_pin_pages_without_live_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let (store, descriptors) = fixture(&path, false);
    drop(store);
    let mut store = unsafe { Store::open(&path).unwrap() };
    assert!(store.live_segments.lock().unwrap().is_empty());
    // This explicit set represents a fully reconstructed fixture root. It is
    // supplied by the test, not automatically decoded from application metadata.
    let retained = BTreeSet::from([0, 69]);
    let first = sweep_unpublished(&mut store, &retained).unwrap();
    assert_eq!(first.pages.len(), 3);
    reuse_pages(&mut store, &first.pages);
    for id in &retained {
        assert_eq!(&*store.bind(&descriptors[*id as usize]).unwrap(), &body(*id));
    }
    assert!(store.live_segments.lock().unwrap().is_empty());
    // Releasing one historical root makes its whole page collectible, without
    // touching the still-retained root's page.
    let released = sweep_unpublished(&mut store, &BTreeSet::from([0])).unwrap();
    assert_eq!(released.pages, BTreeSet::from([descriptors[69].page]));
    reuse_pages(&mut store, &released.pages);
    assert_eq!(&*store.bind(&descriptors[0]).unwrap(), &body(0));
    println!("MEASUREMENT {{\"scenario\":\"restored_explicit_roots\",\"live_leases\":0,\"initial_reclaimed\":3,\"after_one_root_release\":1,\"root_discovery_implemented\":false}}");
}

#[test]
fn opaque_public_objects_and_heads_each_refuse_after_fresh_reopen() {
    for root in [1, 3, 4] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { Store::open(&path).unwrap() };
        if root != 1 { store.init_derived_v1().unwrap(); }
        let (descriptor, bytes) = store.insert_bound(0, 256, &mut |out| out.copy_from_slice(&body(0))).unwrap();
        match root {
            1 => store.publish(0, b"opaque without encoded edge list", &[descriptor.clone()]).unwrap(),
            3 => store.derived_put(0, b"opaque object may refer to payload").unwrap(),
            4 => store.derived_publish_head(0, b"opaque head may refer to payload", &[]).unwrap(),
            _ => unreachable!(),
        }
        drop(bytes);
        drop(store);
        let mut store = unsafe { Store::open(&path).unwrap() };
        assert!(store.live_segments.lock().unwrap().is_empty());
        if root == 1 {
            assert_eq!(store.published(0).unwrap().unwrap(), b"opaque without encoded edge list");
        }
        let before = std::fs::read(&path).unwrap();
        let error = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap_err();
        assert!(error.to_string().contains("opaque published or derived records"));
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert_eq!(&*store.bind(&descriptor).unwrap(), &body(0));
        println!("MEASUREMENT {{\"scenario\":\"opaque_root_after_reopen\",\"root_slot\":{root},\"live_leases\":0,\"refused_without_writes\":true}}");
    }
}

#[test]
fn characterization_retired_identity_reuse_can_reauthenticate_stale_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut store = unsafe { Store::open(&path).unwrap() };
    let mut seen = BTreeMap::<u64, (Descriptor, u8)>::new();
    // Intentional counterexample, not a supported lifetime: a bare Descriptor
    // is not a lease or a registered root. The allocator only rejects IDs still
    // in root 0. With reclamation bolted on, old IDs can name different bytes.
    for generation in 0..64u8 {
        let fill = b'!' + generation;
        let (descriptor, bytes) = store.insert_bound(7, 256, &mut |out| out.fill(fill)).unwrap();
        if let Some((stale, original)) = seen.get(&descriptor.page) {
            assert_eq!(stale, &descriptor);
            assert_ne!(*original, fill);
            assert_eq!(&*store.bind(stale).unwrap(), vec![fill; 256]);
            println!("MEASUREMENT {{\"scenario\":\"retired_id_aba_counterexample\",\"generation\":{generation},\"stale_descriptor_accepted_different_bytes\":true,\"production_gc_safe\":false}}");
            return;
        }
        seen.insert(descriptor.page, (descriptor, fill));
        drop(bytes);
        let swept = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
        assert_eq!(swept.retired_ids, BTreeSet::from([7]));
        assert!(store.get(7).is_err());
    }
    panic!("pinned allocator did not reproduce descriptor ABA within 64 generations");
}

#[test]
fn abort_before_reclamation_commit_preserves_logical_index_and_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let (mut store, descriptors) = fixture(&path, false);
    let retired_page = descriptors[0].page;
    {
        let mut txn = sanakirja::Env::mut_txn_begin(&store.owner.env).unwrap();
        let mut db = txn.root_db::<u64, u64, sanakirja::btree::page::Page<u64, u64>>(0).unwrap();
        for descriptor in descriptors.iter().filter(|d| d.page == retired_page) {
            assert!(sanakirja::btree::del(&mut txn, &mut db, &descriptor.id, None).unwrap());
        }
        assert_eq!(unsafe { txn.decr_rc(retired_page).unwrap() }, 0);
        txn.set_root(0, db.db.into());
        // decr_rc queues committed pages, not same-transaction owned frees.
        for _ in 0..4 {
            assert_ne!(unsafe { txn.alloc_page().unwrap() }.0.offset, retired_page);
        }
        // Abort BEFORE commit. This is not a failed-commit or crash simulation.
    }
    for descriptor in &descriptors {
        assert_eq!(&*store.bind(descriptor).unwrap(), &body(descriptor.id));
    }
    // All original pages are still allocated and can be swept once, not twice.
    let swept = sweep_unpublished(&mut store, &BTreeSet::new()).unwrap();
    assert_eq!(swept.pages.len(), 5);
    reuse_pages(&mut store, &swept.pages);
    drop(store);
    let reopened = unsafe { Store::open(&path).unwrap() };
    assert!(descriptors.iter().all(|d| reopened.bind(d).is_err()));
    println!("MEASUREMENT {{\"scenario\":\"abort_before_sweep_commit\",\"restored_logical_extents\":70,\"next_successful_sweep\":5,\"crash_test\":false}}");
}

#[test]
fn characterization_pinned_engine_remove_root_keeps_committed_root() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw-engine-fixture");
    {
        let env = sanakirja::Env::new(&path, (64 * PAGE) as u64, 2).unwrap();
        let mut txn = sanakirja::Env::mut_txn_begin(&env).unwrap();
        let mut db = unsafe { sanakirja::btree::create_db::<_, u64, u64>(&mut txn).unwrap() };
        sanakirja::btree::put(&mut txn, &mut db, &10, &20).unwrap();
        txn.set_root(5, db.db.into());
        txn.commit().unwrap();
        let mut txn = sanakirja::Env::mut_txn_begin(&env).unwrap();
        txn.remove_root(5);
        txn.commit().unwrap();
    }
    let env = sanakirja::Env::new(&path, (64 * PAGE) as u64, 2).unwrap();
    let txn = sanakirja::Env::txn_begin(&env).unwrap();
    // Pinned 1.4.3 commit skips zero pending roots, so remove_root does not
    // implement durable root retirement. Keep an explicit registry/tombstone.
    let db = txn.root_db::<u64, u64, sanakirja::btree::page::Page<u64, u64>>(5).unwrap();
    assert_eq!(sanakirja::btree::get(&txn, &db, &10, None).unwrap(), Some((&10, &20)));
    println!("MEASUREMENT {{\"scenario\":\"engine_remove_root_counterexample\",\"root_still_present_after_reopen\":true}}");
}
