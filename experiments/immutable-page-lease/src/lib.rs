//! Qualification-only immutable Sanakirja payload pages. No payload reclamation.
use anyhow::{ensure, Result};
use append_only_bytes::{BytesSlice, ImmutableBytes};
use sanakirja::{AllocPage, Commit, LoadPage, RootDb};
use std::{path::Path, ptr::NonNull, sync::Arc};

const PAGE: usize = 4096;
const HEADER: usize = 32;
const MAGIC: u64 = 0x494d4d555441424c;

/// Exclusive database wrapper. Every committed payload allocation is retained
/// forever. Metadata B-tree pages can be reclaimed normally; payload page offsets
/// are plain values and are never passed to any reclamation API.
pub struct Store {
    env: Arc<sanakirja::Env>,
}

struct Lease {
    _env: Arc<sanakirja::Env>,
    ptr: NonNull<u8>,
    len: usize,
}
// SAFETY: The only mutable references to this payload existed before commit.
// Store never mutates or frees committed payload pages. Env retains all mmap
// chunks until its last Arc dies; growth appends mappings rather than replacing
// them in pinned Sanakirja 1.4.3. The unsafe Store::open contract excludes outside
// writers, truncation, compaction, and duplicate mappings for this entire lifetime.
unsafe impl Send for Lease {}
// SAFETY: Same immutable-page and mapping ownership invariant as Send above.
unsafe impl Sync for Lease {}
// SAFETY: See Send above. Bytes are initialized, immutable and address-stable
// through the lifetime of this lease, including across database transactions.
unsafe impl ImmutableBytes for Lease {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: The committed page range is initialized and retained by _env.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Store {
    /// Open a qualification store with exclusive ownership of its backing file.
    ///
    /// # Safety
    /// Caller must ensure no other environment maps this file and no external
    /// process or API mutates, replaces, truncates, compacts or deletes it while
    /// this Store OR ANY derived BytesSlice remains alive. Existing files must
    /// have been produced solely by this wrapper. This contract also applies
    /// after dropping Store while a Loro document still retains leases.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            env: Arc::new(sanakirja::Env::new(path, 64 * 4096, 2)?),
        })
    }

    /// Generate directly in one database page, commit, then transfer a lease.
    /// ID must be fresh; payload is never changed after publication.
    pub fn insert(
        &mut self,
        id: u64,
        len: usize,
        generate: &mut dyn FnMut(&mut [u8]),
    ) -> Result<BytesSlice> {
        ensure!(
            len > 0 && len <= PAGE - HEADER,
            "one nonempty page-bounded extent required"
        );
        let mut txn = sanakirja::Env::mut_txn_begin(&*self.env)?;
        let mut db: sanakirja::btree::Db<u64, u64> = match txn.root_db(0) {
            Some(db) => db,
            // SAFETY: Fresh B-tree in the exclusively held write transaction.
            None => unsafe { sanakirja::btree::create_db(&mut txn)? },
        };
        ensure!(
            sanakirja::btree::get(&txn, &db, &id, None)?.is_none_or(|(key, _)| *key != id),
            "duplicate extent identity"
        );
        // SAFETY: The mutable transaction exclusively owns this fresh page.
        let page = unsafe { txn.alloc_page()? };
        // SAFETY: Allocated pages have PAGE initialized writable bytes. We fill
        // the page before any immutable lease exists; no alias is retained here.
        let raw = unsafe { std::slice::from_raw_parts_mut(page.0.data, PAGE) };
        raw.fill(0);
        raw[8..16].copy_from_slice(&MAGIC.to_le_bytes());
        raw[16..24].copy_from_slice(&(len as u64).to_le_bytes());
        raw[24..32].copy_from_slice(&id.to_le_bytes());
        generate(&mut raw[HEADER..HEADER + len]);
        std::str::from_utf8(&raw[HEADER..HEADER + len])?;
        let ptr = NonNull::new(raw[HEADER..].as_mut_ptr()).unwrap();
        sanakirja::btree::put(&mut txn, &mut db, &id, &page.0.offset)?;
        txn.set_root(0, db.db.into());
        txn.commit()?;
        // Only committed pages escape. A panic/error before commit exposes no
        // pointer and the aborted transaction may reclaim its private page.
        Ok(BytesSlice::from_immutable(Arc::new(Lease {
            _env: self.env.clone(),
            ptr,
            len,
        })))
    }

    /// Reopen a committed extent using its stable logical identity.
    pub fn get(&self, id: u64) -> Result<BytesSlice> {
        let txn = sanakirja::Env::txn_begin(&*self.env)?;
        let db: sanakirja::btree::Db<u64, u64> = txn
            .root_db(0)
            .ok_or_else(|| anyhow::anyhow!("missing root"))?;
        let (found, offset) = sanakirja::btree::get(&txn, &db, &id, None)?
            .ok_or_else(|| anyhow::anyhow!("missing extent"))?;
        ensure!(*found == id, "missing extent");
        // SAFETY: Only this wrapper creates the store, and the offset is a
        // committed payload page that this wrapper never reclaims.
        let page = unsafe { txn.load_page(*offset)? };
        // SAFETY: The mapped page is PAGE bytes long and retained by env.
        let raw = unsafe { std::slice::from_raw_parts(page.data, PAGE) };
        ensure!(
            u64::from_le_bytes(raw[8..16].try_into()?) == MAGIC,
            "invalid payload page"
        );
        let len = usize::try_from(u64::from_le_bytes(raw[16..24].try_into()?))?;
        ensure!(len > 0 && len <= PAGE - HEADER, "invalid payload length");
        ensure!(
            u64::from_le_bytes(raw[24..32].try_into()?) == id,
            "extent identity mismatch"
        );
        std::str::from_utf8(&raw[HEADER..HEADER + len])?;
        let ptr = NonNull::new(raw[HEADER..].as_ptr().cast_mut()).unwrap();
        drop(txn);
        Ok(BytesSlice::from_immutable(Arc::new(Lease {
            _env: self.env.clone(),
            ptr,
            len,
        })))
    }
}

pub struct Allocator<'a> {
    pub store: &'a mut Store,
    pub id: u64,
    pub generated_bytes: usize,
    pub owner_copy_bytes: usize,
    pub initialized_page_bytes: usize,
}
impl loro::TextExtentAllocator for Allocator<'_> {
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
