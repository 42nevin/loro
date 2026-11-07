//! S-direct qualification storage. See store module for the ownership contract.
mod store;
pub use store::{Store,Descriptor,Metrics};
use append_only_bytes::BytesSlice;
const PAGE:usize=4096;
pub struct Allocator<'a> {
    pub store: &'a mut Store,
    pub id: u64,
    pub generated_bytes: usize,
    pub owner_copy_bytes: usize,
    pub initialized_page_bytes: usize,
}
impl loro::TextExtentAllocator for Allocator<'_> {
    fn allocate_text(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> std::io::Result<loro::TextSlice> {
        let bytes = self.allocate(len, generate)?;
        loro::TextSlice::direct_extent(self.id, bytes)
    }
    fn allocate(
        &mut self,
        len: usize,
        generate: &mut dyn FnMut(&mut [u8]),
    ) -> std::io::Result<BytesSlice> {
        let result = self
            .store
            .insert(self.id, len, generate)
            .map_err(std::io::Error::other)?;
        self.generated_bytes += len;
        self.initialized_page_bytes += PAGE;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::{ExportMode, LoroDoc};
    fn append(doc: &LoroDoc, store: &mut Store, id: u64, pos: usize, text: &str) {
        let mut alloc = Allocator {
            store,
            id,
            generated_bytes: 0,
            owner_copy_bytes: 0,
            initialized_page_bytes: 0,
        };
        let mut generated_ptr = 0;
        doc.get_text("text")
            .insert_generated_unicode(pos, text.len(), &mut alloc, |dst| {
                generated_ptr = dst.as_ptr() as usize;
                let mut offset = 0;
                for scalar in text.chars() {
                    let len = scalar.len_utf8();
                    scalar.encode_utf8(&mut dst[offset..offset + len]);
                    offset += len;
                }
            })
            .unwrap();
        assert_eq!(alloc.generated_bytes, text.len());
        assert_eq!(alloc.owner_copy_bytes, 0);
        assert_eq!(alloc.initialized_page_bytes, 4096);
        assert_eq!(
            alloc.store.get(id).unwrap().as_ptr() as usize,
            generated_ptr
        );
        doc.commit();
        let handles = doc.inspect_text_op_handles();
        let operand = handles.iter().find(|slice| slice.backing_info().extent == Some(id)).expect("actual retained direct operand");
        assert_eq!(operand.backing_info().kind, "direct_extent");
        assert_eq!(operand.direct_bytes().unwrap().as_ptr() as usize, generated_ptr);
        let before = alloc.store.metrics();
        for _ in 0..100 { assert_eq!(operand.direct_bytes().unwrap(), text.as_bytes()); }
        assert_eq!(before, alloc.store.metrics());
    }
    #[test]
    fn committed_lease_survives_writes_growth_store_drop_and_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        // SAFETY: Fresh private tempfile, no other mappings or writers.
        let mut store = unsafe { Store::open(&path).unwrap() };
        let mut generated_ptr = 0;
        let owner = store
            .insert(0, 4, &mut |out| {
                generated_ptr = out.as_ptr() as usize;
                out.copy_from_slice("a玉".as_bytes());
            })
            .unwrap();
        assert_eq!(owner.as_ptr() as usize, generated_ptr);
        let initial_size = std::fs::metadata(&path).unwrap().len();
        for id in 1..180 {
            let x = store.insert(id, 4050, &mut |out| out.fill(b'x')).unwrap();
            assert_eq!(x.len(), 4050);
            assert_eq!(&*owner, "a玉".as_bytes());
        }
        let final_size = std::fs::metadata(&path).unwrap().len();
        assert!(final_size > initial_size);
        println!("lease_sample extents=180 payload_bytes={} initialized_page_bytes={} owner_copy_bytes=0 initial_file_bytes={} final_file_bytes={} retained_pointer_equal=true", 4 + 179 * 4050, 180 * PAGE, initial_size, final_size);
        let reloaded = store.get(0).unwrap();
        assert_eq!(reloaded.as_ptr(), owner.as_ptr());
        drop(store);
        let other = owner.clone();
        std::thread::spawn(move || assert_eq!(&*other, "a玉".as_bytes()))
            .join()
            .unwrap();
        assert_eq!(&*owner, "a玉".as_bytes());
        drop(reloaded);
        drop(owner);
        // SAFETY: All leases and the sole previous environment were dropped.
        let store = unsafe { Store::open(&path).unwrap() };
        assert_eq!(&*store.get(0).unwrap(), "a玉".as_bytes());
    }
    #[test]
    fn loro_native_sync_cuts_forks_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        // SAFETY: Fresh private tempfile, no other mappings or writers.
        let mut store = unsafe { Store::open(&path).unwrap() };
        let doc = LoroDoc::new();
        doc.set_peer_id(7).unwrap();
        append(&doc, &mut store, 1, 0, "a");
        let cut = doc.oplog_frontiers();
        let vv = doc.oplog_vv();
        let snapshot = doc.export(ExportMode::Snapshot).unwrap();
        let fork = doc.fork_cow().unwrap();
        append(&fork, &mut store, 2, 1, "🙂");
        assert_eq!(doc.get_text("text").to_string(), "a");
        assert_eq!(fork.get_text("text").to_string(), "a🙂");
        drop(fork); // Abandoning a CoW branch releases its leases, not its durable page.
        drop(doc);
        drop(store);
        // SAFETY: Every previous Loro document and owner has been dropped.
        let mut store = unsafe { Store::open(&path).unwrap() };
        assert_eq!(&*store.get(1).unwrap(), b"a");
        assert_eq!(&*store.get(2).unwrap(), "🙂".as_bytes());
        let doc = LoroDoc::new();
        doc.import(&snapshot).unwrap();
        doc.set_peer_id(7).unwrap();
        append(&doc, &mut store, 3, 1, "玉");
        assert_eq!(doc.get_text("text").to_string(), "a玉");
        let peer = LoroDoc::new();
        peer.import(&snapshot).unwrap();
        peer.import(&doc.export(ExportMode::updates(&vv)).unwrap())
            .unwrap();
        assert_eq!(peer.get_text("text").to_string(), "a玉");
        let delta = doc.export_json_updates(&vv, &doc.oplog_vv());
        assert_eq!(delta.changes.iter().map(|x| x.op_len()).sum::<usize>(), 1);
        doc.checkout(&cut).unwrap();
        assert_eq!(doc.get_text("text").to_string(), "a");
        doc.checkout_to_latest();
        assert_eq!(doc.get_text("text").to_string(), "a玉");
    }
    #[test]
    fn failed_or_panicking_generation_publishes_no_lease() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: Fresh private tempfile, no other mappings or writers.
        let mut store = unsafe { Store::open(&dir.path().join("data")).unwrap() };
        let retained = store.insert(99, 1, &mut |out| out[0] = b'z').unwrap();
        let ptr = retained.as_ptr();
        assert!(store.insert(1, 1, &mut |out| out[0] = 0xff).is_err());
        assert!(store.get(1).is_err());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.insert(2, 1, &mut |_out| panic!("abandon reservation"));
        }));
        assert!(unwind.is_err());
        assert!(store.get(2).is_err());
        let a = store.insert(1, 1, &mut |out| out[0] = b'a').unwrap();
        assert_eq!(&*a, b"a");
        assert_eq!(&*retained, b"z");
        assert_eq!(retained.as_ptr(), ptr);
    }
}
