//! Append-only, sealed object files outside LMDB's snapshot/recycling lifetime.
use super::*;
fn checksum(bytes:&[u8])->u64 {bytes.iter().fold(0xcbf29ce484222325,|h,b|(h^u64::from(*b)).wrapping_mul(0x100000001b3))}
impl Store {
 fn object_path(&self,record:&[u8])->Result<std::path::PathBuf>{
  record_len(record)?;
  let name=record[..16].iter().map(|b|format!("{b:02x}")).collect::<String>();
  Ok(self.owner.path.join("objects").join(name))
 }
 fn check_object(&self,record:&[u8])->Result<File>{
  let file=File::open(self.object_path(record)?)?;
  ensure!(file.metadata()?.len()==record_len(record)? as u64,"object file length mismatch");Ok(file)
 }
pub(super) fn validate_public_object(&self,id:u64,record:&[u8])->Result<()> {
  let file=self.check_object(record)?;
  // Live leases are validation witnesses under the immutable-file contract.
  if let Some(lease)=self.cache.lock().unwrap().get(&id).and_then(Weak::upgrade) {
   ensure!(lease.len==record_len(record)?,"extent length mismatch");return Ok(());
  }
  // Cold publication validates without heap body materialization. This scan is
  // necessary unless a previously validated owner is still alive.
  let map=unsafe{memmap2::MmapOptions::new().map(&file)?};
  ensure!(checksum(&map)==u64::from_be_bytes(record[24..].try_into()?),"object checksum mismatch");std::str::from_utf8(&map)?;Ok(())
 }
 pub fn insert_bound(&mut self,id:u64,len:usize,generate:&mut dyn FnMut(&mut[u8]))->Result<(Descriptor,BytesSlice)> {
  ensure!(len>0 && len<=4064,"extent size outside matched control range");
  let mut txn=self.owner.env.write_txn()?;let k=key(b't',id);
  ensure!(self.owner.db.get(&txn,&k)?.is_none(),"duplicate extent ID");
  let mut record=[0u8;32];File::open("/dev/urandom")?.read_exact(&mut record[..16])?;
  record[16..24].copy_from_slice(&(len as u64).to_be_bytes());
  // Unique create_new files are never overwritten. A failed/crashed generation may
  // leave an orphan, which is not indexed or public and does not prevent ID retry.
  let file=std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(self.object_path(&record)?)?;
  file.set_len(len as u64)?;
  // SAFETY: fresh private file, unpublished, no competing references/writers.
  let mut map=unsafe {memmap2::MmapOptions::new().map_mut(&file)?};
  map.fill(0);generate(&mut map);std::str::from_utf8(&map)?;
  record[24..].copy_from_slice(&checksum(&map).to_be_bytes());
  map.flush()?;
  // Durability order: content + file length, then directory entry, THEN LMDB
  // reference commit. No published reference can precede the durable object.
  file.sync_all()?;File::open(self.owner.path.join("objects"))?.sync_all()?;
  let map=map.make_read_only()?;drop(file); // no writable mapping/handle retained
  self.owner.db.put(&mut txn,&k,&record)?;txn.commit()?;
  let ptr=NonNull::new(map.as_ptr() as *mut u8).unwrap();
  let lease=Arc::new(Lease{_map:map,_owner:self.owner.clone(),ptr,len});
  self.cache.lock().unwrap().insert(id,Arc::downgrade(&lease));
  let c=&self.owner.counts;c.private.fetch_add(1,Ordering::Relaxed);c.initialized.fetch_add(len as u64,Ordering::Relaxed);
  // No reserve/read-transaction rebind in this arm. mprotect seals the same map.
  c.resolutions.fetch_add(1,Ordering::Relaxed);
  let d=Descriptor{store_id:self.store_id(),id,page:id,len};Ok((d,BytesSlice::from_immutable(lease)))
 }
 pub fn bind(&self,d:&Descriptor)->Result<BytesSlice>{
  ensure!(d.store_id==self.store_id() && d.page==d.id,"foreign or forged descriptor");
  let mut cache=self.cache.lock().unwrap();
  if let Some(lease)=cache.get(&d.id).and_then(Weak::upgrade){ensure!(lease.len==d.len,"extent length mismatch");return Ok(BytesSlice::from_immutable(lease));}
  let txn=self.read()?;
  let record=self.owner.db.get(&txn,&key(b't',d.id))?.ok_or_else(||anyhow::anyhow!("missing extent"))?;
  ensure!(record_len(record)?==d.len && d.len>0,"extent length mismatch");
  let file=self.check_object(record)?;
  // SAFETY: unsafe open contract excludes mutation/truncate/unlink for all leases.
  let map=unsafe {memmap2::MmapOptions::new().map(&file)?};
  ensure!(checksum(&map)==u64::from_be_bytes(record[24..].try_into()?),"object checksum mismatch");std::str::from_utf8(&map)?;
  drop(txn); // the essential difference: no LMDB read epoch held by text owners
  let ptr=NonNull::new(map.as_ptr() as *mut u8).unwrap();let len=map.len();
  let lease=Arc::new(Lease{_map:map,_owner:self.owner.clone(),ptr,len});
  self.owner.counts.resolutions.fetch_add(1,Ordering::Relaxed);cache.insert(d.id,Arc::downgrade(&lease));Ok(BytesSlice::from_immutable(lease))
 }
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn detached_owner_survives_churn_without_pinning_lmdb() {
  let dir=tempfile::tempdir().unwrap();let p=dir.path().join("db");
  let mut store=unsafe{Store::open(&p).unwrap()};let mut generated=0;
  let (d,b)=store.insert_bound(1,4,&mut |out|{generated=out.as_ptr() as usize;out.copy_from_slice(b"text")}).unwrap();
  assert_eq!(generated,b.as_ptr() as usize);
  let b2=store.bind(&d).unwrap();assert_eq!(b.as_ptr(),b2.as_ptr());
  for _ in 0..64{store.diagnostic_churn(&[7;16384]).unwrap();}
  let warm=store.diagnostic_last_page_number();
  for _ in 0..512{store.diagnostic_churn(&[8;16384]).unwrap();}
  assert!(store.diagnostic_last_page_number()<=warm+16);
  store.publish(1,b"record",&[d]).unwrap();drop(store);
  assert_eq!(&b[..],b"text");drop(b2);drop(b);
  let reopened=unsafe{Store::open(&p).unwrap()};assert_eq!(&reopened.get(1).unwrap()[..],b"text");
 }
 #[test]
 fn missing_file_cannot_be_published() {
  let dir=tempfile::tempdir().unwrap();let p=dir.path().join("db");
  let mut store=unsafe{Store::open(&p).unwrap()};let(d,b)=store.insert_bound(1,4,&mut |out|out.copy_from_slice(b"text")).unwrap();drop(b);
  // Fault injection before obtaining any lease, not concurrent mapped mutation.
  let entry=std::fs::read_dir(p.join("objects")).unwrap().next().unwrap().unwrap();std::fs::remove_file(entry.path()).unwrap();
  assert!(store.publish(1,b"record",&[d]).is_err());assert!(store.published(1).unwrap().is_none());
 }
}
