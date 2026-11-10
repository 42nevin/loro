//! TEST-ONLY whole-page retirement for a closed-world, unpublished fixture.
//! This is not an application GC or a supported Store API. Public/derived
//! records are refused: their opaque metadata has no root enumerator here.
use super::*;
use loro::{LoroDoc, TextExtentAllocator, TextSlice};
use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Default)]
struct Sweep {
    pages: BTreeSet<u64>,
    retired_ids: BTreeSet<u64>,
}

fn sweep_unpublished(store: &mut Store, retained: &BTreeSet<u64>) -> Result<Sweep> {
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
fn reuse_pages(store: &mut Store, expected: &BTreeSet<u64>) -> BTreeSet<u64> {
    if expected.is_empty() { return BTreeSet::new(); }
    let mut txn = sanakirja::Env::mut_txn_begin(&store.owner.env).unwrap();
    let mut reused = BTreeSet::new();
    for _ in 0..32 {
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
