//! Single-owner immutable payload pages and atomic public records.
//! No payload or metadata blob page is reclaimed in this experiment.
use anyhow::{ensure, Result};
use append_only_bytes::{BytesSlice, ImmutableBytes};
use sanakirja::{AllocPage, Commit, LoadPage, RootDb};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::Path,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
const PAGE: usize = 4096;
const HEADER: usize = 32;
const MAGIC: u64 = 0x494d4d555441424c;
const RECORD_MAGIC: u64 = 0x5344495245435431;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Descriptor {
    pub store_id: [u8; 16],
    pub id: u64,
    pub page: u64,
    pub len: usize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Metrics {
    pub resolutions: u64,
    pub read_transactions: u64,
    pub private_commits: u64,
    pub public_commits: u64,
    pub initialized_page_bytes: u64,
}
#[derive(Default)]
struct Counters {
    resolutions: AtomicU64,
    reads: AtomicU64,
    private: AtomicU64,
    public: AtomicU64,
    initialized: AtomicU64,
}
struct Owner {
    env: sanakirja::Env,
    // Cooperative exclusion survives Store drop because every segment owns Owner.
    _file_lock: File,
    _path_lock: File,
    counts: Counters,
}
pub struct Store {
    owner: Arc<Owner>,
    store_id: [u8; 16],
}
struct SealedSegment {
    _owner: Arc<Owner>,
    ptr: NonNull<u8>,
    len: usize,
}
// SAFETY: Only newly allocated private pages are mutated; no public API exposes
// transactions or frees pages. Committed payload pages are never reused. Owner
// retains Env's append-only mappings and the file lock throughout all leases.
// The unsafe open contract excludes noncooperating mutation/truncation/replacement.
unsafe impl Send for SealedSegment {}
// SAFETY: Same immutable page and lifetime invariant as Send.
unsafe impl Sync for SealedSegment {}
// SAFETY: Pinned Sanakirja 1.4.3 retains old mappings on growth. Page payload is
// initialized once before commit and is immutable for this owner's whole lifetime.
unsafe impl ImmutableBytes for SealedSegment {
    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}
// Linux OFD record locks are separate from the flock locks Sanakirja uses for
// write transactions. Retaining a flock on the data file would deadlock commit.
fn lock_inode(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let lock = libc::flock {
        l_type: libc::F_WRLCK as _,
        l_whence: libc::SEEK_SET as _,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    // SAFETY: valid owned FD and initialized flock struct; no pointer escapes.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Store {
    /// # Safety
    /// The caller must exclude all noncooperating writers, unlink/replacement,
    /// truncation, compaction and corruption for this Store AND every derived
    /// lease. Existing files must originate exclusively from this wrapper.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        let lock_path = path.with_extension("s-direct-lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        // Existing files are inode-locked before mapping, catching hardlink and
        // symlink aliases. Fresh creation is serialized by the path lock; the
        // unsafe contract excludes external alias races during initialization.
        let existing = path.exists();
        let inode_lock = if existing {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            lock_inode(&file)?;
            let length = file.metadata()?.len();
            // Env::new rounds/grows files before validating their header. Every
            // file created by this wrapper is already at least this size and
            // page aligned. Refuse malformed existing files before mapping.
            ensure!(length >= (64 * PAGE) as u64 && length % PAGE as u64 == 0,
                "existing Store has invalid file length");
            Some(file)
        } else {
            None
        };
        let env = sanakirja::Env::new(path, (64 * PAGE) as u64, 2)?;
        let inode_lock = match inode_lock {
            Some(file) => file,
            None => {
                let file = OpenOptions::new().read(true).write(true).open(path)?;
                lock_inode(&file)?;
                file
            }
        };
        let owner = Arc::new(Owner {
            env,
            _file_lock: inode_lock,
            _path_lock: lock,
            counts: Counters::default(),
        });
        // A mutable transaction copies Sanakirja's active root into its next
        // root page even when aborted. Existing identity lookup must therefore
        // use a read transaction, including when format admission later refuses.
        let existing_id = {
            let txn = sanakirja::Env::txn_begin(&owner.env)?;
            if let Some(db) = txn.root_db::<u64, u64, sanakirja::btree::page::Page<u64, u64>>(2) {
                let mut id = [0u8; 16];
                for key in 0..2u64 {
                    let (found, v) = sanakirja::btree::get(&txn, &db, &key, None)?
                        .ok_or_else(|| anyhow::anyhow!("missing store identity"))?;
                    ensure!(*found == key, "invalid store identity");
                    id[(key as usize) * 8..(key as usize + 1) * 8]
                        .copy_from_slice(&v.to_le_bytes());
                }
                Some(id)
            } else { None }
        };
        let store_id = if let Some(id) = existing_id { id } else {
            ensure!(!existing, "existing Store has no identity; refusing initialization");
            let mut txn = sanakirja::Env::mut_txn_begin(&owner.env)?;
            let mut id = [0u8; 16];
            File::open("/dev/urandom")?.read_exact(&mut id)?;
            let mut db = unsafe { sanakirja::btree::create_db::<_, u64, u64>(&mut txn)? };
            for key in 0..2u64 {
                let v = u64::from_le_bytes(id[key as usize * 8..(key as usize + 1) * 8].try_into()?);
                sanakirja::btree::put(&mut txn, &mut db, &key, &v)?;
            }
            txn.set_root(2, db.db.into());
            txn.commit()?;
            id
        };
        Ok(Self { owner, store_id })
    }
    pub fn store_id(&self) -> [u8; 16] {
        self.store_id
    }
    pub fn metrics(&self) -> Metrics {
        let c = &self.owner.counts;
        Metrics {
            resolutions: c.resolutions.load(Ordering::SeqCst),
            read_transactions: c.reads.load(Ordering::SeqCst),
            private_commits: c.private.load(Ordering::SeqCst),
            public_commits: c.public.load(Ordering::SeqCst),
            initialized_page_bytes: c.initialized.load(Ordering::SeqCst),
        }
    }
    pub fn insert_bound(
        &mut self,
        id: u64,
        len: usize,
        generate: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(Descriptor, BytesSlice)> {
        ensure!(
            len > 0 && len <= PAGE - HEADER,
            "one nonempty page-bounded extent required"
        );
        let mut txn = sanakirja::Env::mut_txn_begin(&self.owner.env)?;
        let mut db: sanakirja::btree::Db<u64, u64> = match txn.root_db(0) {
            Some(db) => db,
            None => unsafe { sanakirja::btree::create_db(&mut txn)? },
        };
        ensure!(
            sanakirja::btree::get(&txn, &db, &id, None)?.is_none_or(|(k, _)| *k != id),
            "duplicate extent identity"
        );
        let page = unsafe { txn.alloc_page()? };
        let raw = unsafe { std::slice::from_raw_parts_mut(page.0.data, PAGE) };
        raw.fill(0);
        self.owner
            .counts
            .initialized
            .fetch_add(PAGE as u64, Ordering::SeqCst);
        raw[8..16].copy_from_slice(&MAGIC.to_le_bytes());
        raw[16..24].copy_from_slice(&(len as u64).to_le_bytes());
        raw[24..32].copy_from_slice(&id.to_le_bytes());
        generate(&mut raw[HEADER..HEADER + len]);
        std::str::from_utf8(&raw[HEADER..HEADER + len])?;
        let ptr = NonNull::new(raw[HEADER..].as_mut_ptr()).unwrap();
        let descriptor = Descriptor {
            store_id: self.store_id,
            id,
            page: page.0.offset,
            len,
        };
        sanakirja::btree::put(&mut txn, &mut db, &id, &page.0.offset)?;
        txn.set_root(0, db.db.into());
        txn.commit()?;
        self.owner.counts.private.fetch_add(1, Ordering::SeqCst);
        Ok((
            descriptor,
            BytesSlice::from_immutable(Arc::new(SealedSegment {
                _owner: self.owner.clone(),
                ptr,
                len,
            })),
        ))
    }
    pub fn insert(
        &mut self,
        id: u64,
        len: usize,
        generate: &mut dyn FnMut(&mut [u8]),
    ) -> Result<BytesSlice> {
        Ok(self.insert_bound(id, len, generate)?.1)
    }
    fn resolve(&self, id: u64) -> Result<(Descriptor, BytesSlice)> {
        self.owner.counts.resolutions.fetch_add(1, Ordering::SeqCst);
        self.owner.counts.reads.fetch_add(1, Ordering::SeqCst);
        let txn = sanakirja::Env::txn_begin(&self.owner.env)?;
        let db: sanakirja::btree::Db<u64, u64> = txn
            .root_db(0)
            .ok_or_else(|| anyhow::anyhow!("missing root"))?;
        let (key, offset) = sanakirja::btree::get(&txn, &db, &id, None)?
            .ok_or_else(|| anyhow::anyhow!("missing extent"))?;
        ensure!(*key == id, "missing extent");
        let page = unsafe { txn.load_page(*offset)? };
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
        let descriptor = Descriptor {
            store_id: self.store_id,
            id,
            page: *offset,
            len,
        };
        let ptr = NonNull::new(raw[HEADER..].as_ptr().cast_mut()).unwrap();
        drop(txn);
        Ok((
            descriptor,
            BytesSlice::from_immutable(Arc::new(SealedSegment {
                _owner: self.owner.clone(),
                ptr,
                len,
            })),
        ))
    }
    pub fn get(&self, id: u64) -> Result<BytesSlice> {
        Ok(self.resolve(id)?.1)
    }
    pub fn descriptor(&self, id: u64) -> Result<Descriptor> {
        Ok(self.resolve(id)?.0)
    }
    pub fn bind(&self, expected: &Descriptor) -> Result<BytesSlice> {
        ensure!(expected.store_id == self.store_id, "wrong store identity");
        let (actual, bytes) = self.resolve(expected.id)?;
        ensure!(&actual == expected, "conflicting extent descriptor");
        Ok(bytes)
    }
    /// Publish metadata and its reference set atomically in the same environment.
    /// Application receipt/acceptance validation is required before this call.
    /// Duplicate sequence keys are rejected, including byte-identical retries.
    pub fn publish(
        &mut self,
        record_id: u64,
        metadata: &[u8],
        extents: &[Descriptor],
    ) -> Result<()> {
        for descriptor in extents {
            self.bind(descriptor)?;
        }
        self.publish_record(1,RECORD_MAGIC,record_id,metadata)
    }
    fn publish_record(&mut self,root:usize,magic:u64,record_id:u64,metadata:&[u8])->Result<()> {
        ensure!(metadata.len()<=1024*1024*1024,"record exceeds experiment limit");
        let mut txn = sanakirja::Env::mut_txn_begin(&self.owner.env)?;
        let mut db: sanakirja::btree::Db<u64, u64> = match txn.root_db(root) {
            Some(db) => db,
            None => unsafe { sanakirja::btree::create_db(&mut txn)? },
        };
        ensure!(
            sanakirja::btree::get(&txn, &db, &record_id, None)?
                .is_none_or(|(k, _)| *k != record_id),
            "duplicate public record"
        );
        // Copy metadata only; caller deliberately excludes payload bytes. Raw
        // blob offsets are ordinary u64 values, never automatic GC references.
        let chunk_size = PAGE - 40;
        let chunks = metadata.len().max(1).div_ceil(chunk_size);
        let mut pages = Vec::with_capacity(chunks);
        for _ in 0..chunks {
            pages.push(unsafe { txn.alloc_page()? });
        }
        for (i, page) in pages.iter().enumerate() {
            let raw = unsafe { std::slice::from_raw_parts_mut(page.0.data, PAGE) };
            raw.fill(0);
            self.owner
                .counts
                .initialized
                .fetch_add(PAGE as u64, Ordering::SeqCst);
            raw[8..16].copy_from_slice(&magic.to_le_bytes());
            raw[16..24].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
            raw[24..32].copy_from_slice(&pages.get(i + 1).map_or(0, |p| p.0.offset).to_le_bytes());
            raw[32..40].copy_from_slice(&record_id.to_le_bytes());
            let lo = i * chunk_size;
            let hi = (lo + chunk_size).min(metadata.len());
            raw[40..40 + hi - lo].copy_from_slice(&metadata[lo..hi]);
        }
        sanakirja::btree::put(&mut txn, &mut db, &record_id, &pages[0].0.offset)?;
        txn.set_root(root, db.db.into());
        txn.commit()?;
        self.owner.counts.public.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    pub fn published(&self, record_id: u64) -> Result<Option<Vec<u8>>> {
        self.read_record(1,RECORD_MAGIC,record_id)
    }
    fn read_record(&self,root:usize,magic:u64,record_id:u64)->Result<Option<Vec<u8>>> {
        self.owner.counts.reads.fetch_add(1, Ordering::SeqCst);
        let txn = sanakirja::Env::txn_begin(&self.owner.env)?;
        let Some(db) = txn.root_db::<u64, u64, sanakirja::btree::page::Page<u64, u64>>(root) else {
            return Ok(None);
        };
        let Some((key, offset)) = sanakirja::btree::get(&txn, &db, &record_id, None)? else {
            return Ok(None);
        };
        if *key != record_id {
            return Ok(None);
        }
        let mut offset = *offset;
        let mut result = Vec::new();
        let mut expected = None;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            ensure!(offset != 0 && seen.insert(offset), "invalid record chain");
            let page = unsafe { txn.load_page(offset)? };
            let raw = unsafe { std::slice::from_raw_parts(page.data, PAGE) };
            ensure!(
                u64::from_le_bytes(raw[8..16].try_into()?) == magic,
                "invalid record magic"
            );
            ensure!(
                u64::from_le_bytes(raw[32..40].try_into()?) == record_id,
                "record identity mismatch"
            );
            let len = usize::try_from(u64::from_le_bytes(raw[16..24].try_into()?))?;
            ensure!(len <= 1024 * 1024 * 1024, "record exceeds experiment limit");
            ensure!(
                expected.is_none_or(|x| x == len),
                "inconsistent record length"
            );
            expected = Some(len);
            let take = (len - result.len()).min(PAGE - 40);
            result.extend_from_slice(&raw[40..40 + take]);
            offset = u64::from_le_bytes(raw[24..32].try_into()?);
            if result.len() == len {
                ensure!(offset == 0, "excess record chain");
                break;
            }
        }
        Ok(Some(result))
    }
    pub fn published_ids(&self) -> Result<Vec<u64>> {
        self.owner.counts.reads.fetch_add(1, Ordering::SeqCst);
        let txn = sanakirja::Env::txn_begin(&self.owner.env)?;
        let Some(db) = txn.root_db::<u64, u64, sanakirja::btree::page::Page<u64, u64>>(1) else {
            return Ok(Vec::new());
        };
        let mut result = Vec::new();
        for entry in sanakirja::btree::iter(&txn, &db, None)? {
            let (k, _) = entry?;
            result.push(*k);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_owner_and_atomic_public_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = unsafe { Store::open(&path).unwrap() };
        let mut dest = 0;
        let (descriptor, bytes) = store
            .insert_bound(1, 3, &mut |out| {
                dest = out.as_ptr() as usize;
                out.copy_from_slice("玉".as_bytes());
            })
            .unwrap();
        assert!(store.published(1).unwrap().is_none());
        let operand = loro::TextSlice::direct_extent(1, bytes.clone()).unwrap();
        let catalog = loro::TextSlice::direct_extent(1, bytes).unwrap();
        let before = store.metrics();
        for _ in 0..100 {
            let a = operand.direct_bytes().unwrap();
            let b = catalog.direct_bytes().unwrap();
            assert_eq!(a.as_ptr() as usize, dest);
            assert_eq!(a.as_ptr(), b.as_ptr());
            assert_eq!(a, "玉".as_bytes());
        }
        assert_eq!(before, store.metrics());
        let mut wrong = descriptor.clone();
        wrong.page += 4096;
        assert!(store.publish(1, b"no payload", &[wrong]).is_err());
        assert!(store.published(1).unwrap().is_none());
        let metadata = vec![42; 9000];
        store.publish(1, &metadata, &[descriptor.clone()]).unwrap();
        assert_eq!(store.published(1).unwrap(), Some(metadata.clone()));
        assert!(store.publish(1, b"replacement", &[]).is_err());
        assert!(unsafe { Store::open(&path) }.is_err());
        drop(store);
        // Lock must outlive facade through each independent consumer.
        assert!(unsafe { Store::open(&path) }.is_err());
        drop(operand);
        assert!(unsafe { Store::open(&path) }.is_err());
        assert_eq!(catalog.direct_bytes().unwrap(), "玉".as_bytes());
        drop(catalog);
        let store = unsafe { Store::open(&path).unwrap() };
        assert_eq!(store.published_ids().unwrap(), vec![1]);
        assert_eq!(store.published(1).unwrap(), Some(metadata));
        let bytes = store.bind(&descriptor).unwrap();
        assert_eq!(&*bytes, "玉".as_bytes());
    }
}

#[path="derived.rs"]
mod derived;
