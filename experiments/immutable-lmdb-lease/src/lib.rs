//! Experimental immutable objects over unmodified LMDB. No database page identities escape.
//! A committed value is rebound into a pinned read epoch; generation and committed
//! addresses are NOT required to match. Long epochs retain unrelated obsolete pages.
use anyhow::{ensure, Result};
use append_only_bytes::{BytesSlice, ImmutableBytes};
use heed::{types::Bytes, Database, Env, EnvOpenOptions, RoTxn, WithoutTls};
use std::{collections::HashMap, fs::File, io::Read, path::Path, ptr::NonNull, sync::{Arc,Mutex,Weak,atomic::{AtomicU64,Ordering}}};
#[cfg(feature="detached-objects")]
mod detached;
const MAP_SIZE:usize=512*1024*1024;
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub struct Descriptor {pub store_id:[u8;16],pub id:u64,/// Compatibility field: logical object ID, NEVER a physical page.
pub page:u64,pub len:usize}
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq,serde::Serialize)]
pub struct Metrics {pub resolutions:u64,pub read_transactions:u64,pub private_commits:u64,pub public_commits:u64,pub initialized_page_bytes:u64,pub producer_rebindings:u64}
#[derive(Default)]
struct Counts {resolutions:AtomicU64,reads:AtomicU64,private:AtomicU64,public:AtomicU64,initialized:AtomicU64,rebindings:AtomicU64}
struct Owner {#[cfg(feature="detached-objects")] path:std::path::PathBuf,env:Env<WithoutTls>,db:Database<Bytes,Bytes>,churn_db:Database<Bytes,Bytes>,_lock:File,identity:[u8;16],counts:Counts}
struct Lease { #[cfg(not(feature="detached-objects"))] _txn:Mutex<RoTxn<'static,WithoutTls>>, #[cfg(feature="detached-objects")] _map:memmap2::Mmap, _owner:Arc<Owner>, ptr:NonNull<u8>,len:usize }
// NOTLS permits moving the owned transaction. No operation is performed on it
// after construction; Mutex provides Sync without claiming RoTxn itself is Sync.
// The txn/environment are dropped only with the last Arc, after all byte borrows.
// Concurrent byte reads do not invoke LMDB transaction APIs. LMDB MVCC prevents
// reuse of these pages while the read epoch is alive. Map resizing is forbidden.
// In detached mode the Mmap instead owns an immutable sealed object file view;
// no LMDB txn is retained, and external file mutation is excluded by open contract.
unsafe impl Send for Lease {}
unsafe impl Sync for Lease {}
unsafe impl ImmutableBytes for Lease {fn as_bytes(&self)->&[u8] {unsafe {std::slice::from_raw_parts(self.ptr.as_ptr(),self.len)}}}
pub struct Store {owner:Arc<Owner>,cache:Mutex<HashMap<u64,Weak<Lease>>>}
fn key(kind:u8,id:u64)->[u8;9] {let mut k=[0;9];k[0]=kind;k[1..].copy_from_slice(&id.to_be_bytes());k}
impl Store {
/// # Safety
/// All users must exclude uncoordinated mutation, resize, truncation, replacement
/// and unlink of this environment for the lifetime of ALL returned BytesSlices.
/// The cooperative sidecar lock is retained by leases, not just Store. Fixed map
/// size deliberately fails with MapFull rather than invalidating pointers.
pub unsafe fn open(path:&Path)->Result<Self> {
 let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path.with_extension("lease-lock"))?;
 fs2::FileExt::try_lock_exclusive(&lock)?;
 std::fs::create_dir_all(path)?;
 let absolute=std::fs::canonicalize(path)?;let path=absolute.as_path();
 if let Some(parent)=path.parent(){File::open(parent)?.sync_all()?;}
 let mut opts=EnvOpenOptions::new().read_txn_without_tls();opts.map_size(MAP_SIZE).max_dbs(2).max_readers(32768);
 let existed=path.join("data.mdb").exists();
 let env=unsafe {opts.open(path)?};
 let (db,churn_db,identity)=if existed {
   let txn=env.read_txn()?;
   let db:Database<Bytes,Bytes>=env.open_database(&txn,Some("immutable-objects-v1"))?.ok_or_else(||anyhow::anyhow!("unsupported existing store"))?;
   let bytes=db.get(&txn,b"identity-v1")?.ok_or_else(||anyhow::anyhow!("missing store identity"))?;
   ensure!(bytes.len()==16,"malformed identity");
   ensure!(db.get(&txn,b"storage-mode")?==Some(storage_mode()),"storage mode mismatch");
   let churn_db=env.open_database(&txn,Some("diagnostic-unrelated"))?.ok_or_else(||anyhow::anyhow!("missing diagnostic database"))?;
   let identity=bytes.try_into().unwrap();txn.commit()?;
   (db,churn_db,identity)
 } else {
   let mut txn=env.write_txn()?;let db:Database<Bytes,Bytes>=env.create_database(&mut txn,Some("immutable-objects-v1"))?;
   ensure!(db.is_empty(&txn)?,"unsupported existing store");let mut id=[0;16];File::open("/dev/urandom")?.read_exact(&mut id)?;db.put(&mut txn,b"identity-v1",&id)?;db.put(&mut txn,b"storage-mode",storage_mode())?;let churn_db=env.create_database(&mut txn,Some("diagnostic-unrelated"))?;txn.commit()?;(db,churn_db,id)
 };
 #[cfg(feature="detached-objects")]
 {std::fs::create_dir_all(path.join("objects"))?;File::open(path)?.sync_all()?;}
 Ok(Self{owner:Arc::new(Owner{#[cfg(feature="detached-objects")] path:path.to_path_buf(),env,db,churn_db,_lock:lock,identity,counts:Counts::default()}),cache:Mutex::new(HashMap::new())})
}
pub fn store_id(&self)->[u8;16]{self.owner.identity}
pub fn metrics(&self)->Metrics {let c=&self.owner.counts;let get=|a:&AtomicU64|a.load(Ordering::Relaxed);Metrics{resolutions:get(&c.resolutions),read_transactions:get(&c.reads),private_commits:get(&c.private),public_commits:get(&c.public),initialized_page_bytes:get(&c.initialized),producer_rebindings:get(&c.rebindings)}}
fn read(&self)->Result<RoTxn<'static,WithoutTls>> {self.owner.counts.reads.fetch_add(1,Ordering::Relaxed);Ok(self.owner.env.clone().static_read_txn()?)}
#[cfg(not(feature="detached-objects"))]
pub fn insert_bound(&mut self,id:u64,len:usize,generate:&mut dyn FnMut(&mut[u8]))->Result<(Descriptor,BytesSlice)> {
 ensure!(len>0 && len<=4064,"extent size outside matched control range");
 let mut txn=self.owner.env.write_txn()?;let k=key(b't',id);ensure!(self.owner.db.get(&txn,&k)?.is_none(),"duplicate extent ID");
 self.owner.db.put_reserved(&mut txn,&k,len,|space| {space.fill_zeroes();generate(space.written_mut());std::str::from_utf8(space.written_mut()).map_err(std::io::Error::other)?;Ok(())})?;
 txn.commit()?;let c=&self.owner.counts;c.private.fetch_add(1,Ordering::Relaxed);c.initialized.fetch_add(len as u64,Ordering::Relaxed);c.rebindings.fetch_add(1,Ordering::Relaxed);
 let d=Descriptor{store_id:self.store_id(),id,page:id,len};let slice=self.bind(&d)?;Ok((d,slice))
}
pub fn insert(&mut self,id:u64,len:usize,generate:&mut dyn FnMut(&mut[u8]))->Result<BytesSlice>{Ok(self.insert_bound(id,len,generate)?.1)}
pub fn descriptor(&self,id:u64)->Result<Descriptor>{let txn=self.read()?;let value=self.owner.db.get(&txn,&key(b't',id))?.ok_or_else(||anyhow::anyhow!("missing extent"))?;Ok(Descriptor{store_id:self.store_id(),id,page:id,len:record_len(value)?})}
pub fn get(&self,id:u64)->Result<BytesSlice>{self.bind(&self.descriptor(id)?)}
#[cfg(not(feature="detached-objects"))]
pub fn bind(&self,d:&Descriptor)->Result<BytesSlice>{
 ensure!(d.store_id==self.store_id() && d.page==d.id,"foreign or forged descriptor");
 let mut cache=self.cache.lock().unwrap();
 if let Some(lease)=cache.get(&d.id).and_then(Weak::upgrade){ensure!(lease.len==d.len,"extent length mismatch");return Ok(BytesSlice::from_immutable(lease));}
 let txn=self.read()?;let value=self.owner.db.get(&txn,&key(b't',d.id))?.ok_or_else(||anyhow::anyhow!("missing extent"))?;ensure!(value.len()==d.len && !value.is_empty(),"extent length mismatch");std::str::from_utf8(value)?;
 let ptr=NonNull::new(value.as_ptr() as *mut u8).unwrap();let len=value.len();
 let lease=Arc::new(Lease{_txn:Mutex::new(txn),_owner:self.owner.clone(),ptr,len});self.owner.counts.resolutions.fetch_add(1,Ordering::Relaxed);cache.insert(d.id,Arc::downgrade(&lease));Ok(BytesSlice::from_immutable(lease))
}
pub fn publish(&mut self,id:u64,metadata:&[u8],extents:&[Descriptor])->Result<()> {
 ensure!(metadata.len()<=1<<30,"metadata too large");let mut txn=self.owner.env.write_txn()?;let k=key(b'p',id);ensure!(self.owner.db.get(&txn,&k)?.is_none(),"duplicate publication");
 for d in extents {ensure!(d.store_id==self.store_id() && d.page==d.id,"foreign descriptor");let v=self.owner.db.get(&txn,&key(b't',d.id))?.ok_or_else(||anyhow::anyhow!("missing extent"))?;ensure!(record_len(v)?==d.len,"extent length mismatch");
 #[cfg(feature="detached-objects")] self.validate_public_object(d.id,v)?;
 }
 self.owner.db.put(&mut txn,&k,metadata)?;txn.commit()?;self.owner.counts.public.fetch_add(1,Ordering::Relaxed);Ok(())
}
pub fn published(&self,id:u64)->Result<Option<Vec<u8>>>{self.read_record(b'p',id)}
fn read_record(&self,kind:u8,id:u64)->Result<Option<Vec<u8>>>{let txn=self.read()?;Ok(self.owner.db.get(&txn,&key(kind,id))?.map(<[u8]>::to_vec))}
fn ids(&self,kind:u8)->Result<Vec<u64>>{let txn=self.read()?;let result=self.owner.db.prefix_iter(&txn,&[kind])?.map(|entry|{let(k,_)=entry?;ensure!(k.len()==9,"malformed object key");Ok(u64::from_be_bytes(k[1..].try_into()?))}).collect();result}
pub fn published_ids(&self)->Result<Vec<u64>>{self.ids(b'p')}
pub fn init_derived_v1(&mut self)->Result<()> {
 let mut txn=self.owner.env.write_txn()?;ensure!(self.owner.db.get(&txn,b"derived-schema")?.is_none(),"derived schema already initialized");
 for kind in [b't',b'p'] {ensure!(self.owner.db.prefix_iter(&txn,&[kind])?.next().is_none(),"derived schema requires pristine store");}
 self.owner.db.put(&mut txn,b"derived-schema",b"derived-v1")?;txn.commit()?;Ok(())
}
pub fn derived_schema_version(&self)->Result<Option<u64>>{let txn=self.read()?;match self.owner.db.get(&txn,b"derived-schema")? {None=>Ok(None),Some(b"derived-v1")=>Ok(Some(1)),_=>anyhow::bail!("malformed or unsupported derived schema")}}
pub fn require_derived_v1(&self)->Result<()>{ensure!(self.derived_schema_version()?==Some(1),"missing derived schema v1");Ok(())}
pub fn derived_put(&mut self,id:u64,bytes:&[u8])->Result<()>{ensure!(bytes.len()<=1<<30,"metadata too large");self.require_derived_v1()?;let mut txn=self.owner.env.write_txn()?;let k=key(b'o',id);ensure!(self.owner.db.get(&txn,&k)?.is_none(),"duplicate derived object");self.owner.db.put(&mut txn,&k,bytes)?;txn.commit()?;self.owner.counts.public.fetch_add(1,Ordering::Relaxed);Ok(())}
pub fn derived_get(&self,id:u64)->Result<Option<Vec<u8>>>{self.require_derived_v1()?;self.read_record(b'o',id)}
pub fn derived_next_object_id(&self)->Result<u64>{self.require_derived_v1()?;match self.ids(b'o')?.last(){Some(id)=>id.checked_add(1).ok_or_else(||anyhow::anyhow!("object ID exhausted")),None=>Ok(0)}}
pub fn derived_head(&self)->Result<Option<(u64,Vec<u8>)>>{self.require_derived_v1()?;let txn=self.read()?;let mut last=None;for entry in self.owner.db.prefix_iter(&txn,b"h")?{let(k,v)=entry?;ensure!(k.len()==9,"malformed head key");last=Some((u64::from_be_bytes(k[1..].try_into()?),v.to_vec()));}Ok(last)}
pub fn derived_publish_head(&mut self,sequence:u64,bytes:&[u8],required:&[u64])->Result<()>{
 ensure!(bytes.len()<=1<<30,"metadata too large");self.require_derived_v1()?;let mut txn=self.owner.env.write_txn()?;
 for entry in self.owner.db.prefix_iter(&txn,b"h")? {let(k,_)=entry?;ensure!(k.len()==9 && sequence>u64::from_be_bytes(k[1..].try_into()?),"stale derived head");}
 for id in required {ensure!(self.owner.db.get(&txn,&key(b'o',*id))?.is_some(),"missing derived object");}
 self.owner.db.put(&mut txn,&key(b'h',sequence),bytes)?;txn.commit()?;self.owner.counts.public.fetch_add(1,Ordering::Relaxed);Ok(())
}
/// Diagnostic mutable namespace, unrelated to immutable text/derived objects.
/// Holding any lease can delay recycling pages from these overwrites too.
pub fn diagnostic_churn(&mut self,bytes:&[u8])->Result<()> {let mut txn=self.owner.env.write_txn()?;self.owner.churn_db.put(&mut txn,b"churn",bytes)?;txn.commit()?;Ok(())}
pub fn diagnostic_last_page_number(&self)->usize{self.owner.env.info().last_page_number}
}

pub struct Allocator<'a>{pub store:&'a mut Store,pub id:u64,pub generated_bytes:usize,pub owner_copy_bytes:usize,pub initialized_page_bytes:usize}
impl loro::TextExtentAllocator for Allocator<'_>{
 fn allocate_text(&mut self,len:usize,generate:&mut dyn FnMut(&mut[u8]))->std::io::Result<loro::TextSlice>{let bytes=self.allocate(len,generate)?;loro::TextSlice::direct_extent(self.id,bytes)}
 fn allocate(&mut self,len:usize,generate:&mut dyn FnMut(&mut[u8]))->std::io::Result<BytesSlice>{let result=self.store.insert(self.id,len,generate).map_err(std::io::Error::other)?;self.generated_bytes+=len;self.initialized_page_bytes+=len;Ok(result)}
}

#[cfg(not(feature="detached-objects"))]
fn storage_mode()->&'static [u8]{b"pinned-lmdb-v1"}
#[cfg(feature="detached-objects")]
fn storage_mode()->&'static [u8]{b"detached-files-v1"}
#[cfg(not(feature="detached-objects"))]
fn record_len(value:&[u8])->Result<usize>{Ok(value.len())}
#[cfg(feature="detached-objects")]
fn record_len(value:&[u8])->Result<usize>{ensure!(value.len()==32,"invalid detached descriptor");Ok(usize::try_from(u64::from_be_bytes(value[16..24].try_into()?))?)}
