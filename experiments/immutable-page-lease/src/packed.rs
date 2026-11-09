//! Format-v2 packed immutable extents. A batch is sealed as a whole; later
//! batches never append to its unused tail. There is no old-format migration.
use super::*;

impl Store {
    /// Generate a bounded set of extents directly into fresh database pages.
    /// Each generator runs exactly once, in input order. No lease escapes until
    /// every payload is valid and the single private transaction commits.
    /// The batch boundary is explicit: batching unrelated future edits would
    /// require delaying their durability and is not performed by this API.
    pub fn insert_batch_bound(
        &mut self,
        entries: &[(u64, usize)],
        generate: &mut dyn FnMut(usize, &mut [u8]),
    ) -> Result<Vec<(Descriptor, BytesSlice)>> {
        if entries.is_empty() { return Ok(Vec::new()); }
        let mut ids = std::collections::BTreeSet::new();
        for &(id, len) in entries {
            ensure!(len > 0 && len <= PAGE - HEADER, "one nonempty page-bounded extent required");
            ensure!(ids.insert(id), "duplicate extent identity in batch");
        }
        let mut txn = sanakirja::Env::mut_txn_begin(&self.owner.env)?;
        let mut db: sanakirja::btree::Db<u64,u64> = match txn.root_db(0) {
            Some(db) => db,
            None => unsafe { sanakirja::btree::create_db(&mut txn)? },
        };
        // Reject existing identities before invoking any generator.
        for &(id, _) in entries {
            ensure!(sanakirja::btree::get(&txn,&db,&id,None)?.is_none_or(|(key,_)|*key!=id), "duplicate extent identity");
        }
        let mut current = None;
        let mut used = PAGE;
        let mut sealed = Vec::with_capacity(entries.len());
        for (index, &(id,len)) in entries.iter().enumerate() {
            let slot = (used + 7) & !7;
            if slot.checked_add(HEADER + len).is_none_or(|end|end > PAGE) {
                let page = unsafe { txn.alloc_page()? };
                let raw = unsafe { std::slice::from_raw_parts_mut(page.0.data,PAGE) };
                raw.fill(0);
                self.owner.counts.initialized.fetch_add(PAGE as u64,Ordering::SeqCst);
                current = Some((page.0.offset,page.0.data));
                used = 0;
            } else {
                used = slot;
            }
            let (page, data) = current.expect("nonempty extent allocated a page");
            let raw = unsafe { std::slice::from_raw_parts_mut(data,PAGE) };
            raw[used+8..used+16].copy_from_slice(&MAGIC.to_le_bytes());
            raw[used+16..used+24].copy_from_slice(&(len as u64).to_le_bytes());
            raw[used+24..used+32].copy_from_slice(&id.to_le_bytes());
            let offset = used + HEADER;
            generate(index,&mut raw[offset..offset+len]);
            std::str::from_utf8(&raw[offset..offset+len])?;
            let ptr = NonNull::new(raw[offset..].as_mut_ptr()).unwrap();
            let descriptor = Descriptor {store_id:self.store_id,id,page,offset,len};
            let location = page.checked_add(used as u64).ok_or_else(||anyhow::anyhow!("extent location overflow"))?;
            sanakirja::btree::put(&mut txn,&mut db,&id,&location)?;
            sealed.push((descriptor,ptr));
            used = offset + len;
        }
        txn.set_root(0,db.db.into());
        txn.commit()?;
        self.owner.counts.private.fetch_add(1,Ordering::SeqCst);
        // Keep each extent's own identity and lease, even when pages are shared.
        Ok(sealed.into_iter().map(|(descriptor,ptr)| {
            let segment = Arc::new(SealedSegment {
                len:descriptor.len, descriptor:descriptor.clone(),
                cache:Arc::downgrade(&self.live_segments),
                _owner:self.owner.clone(),ptr,
            });
            self.live_segments.lock().unwrap().insert(descriptor.id,Arc::downgrade(&segment));
            (descriptor,BytesSlice::from_immutable(segment))
        }).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pre_v2_store_is_refused_without_rewriting_bytes() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("old");
        {
            let env=sanakirja::Env::new(&path,(64*PAGE) as u64,2).unwrap();
            let mut txn=sanakirja::Env::mut_txn_begin(&env).unwrap();
            let mut db=unsafe{sanakirja::btree::create_db::<_,u64,u64>(&mut txn).unwrap()};
            sanakirja::btree::put(&mut txn,&mut db,&0,&42).unwrap();
            sanakirja::btree::put(&mut txn,&mut db,&1,&43).unwrap();
            txn.set_root(2,db.db.into());txn.commit().unwrap();
        }
        let before=std::fs::read(&path).unwrap();
        let err=unsafe{Store::open(&path)}.err().expect("v1 without payload marker is rejected");
        assert!(err.to_string().contains("requires v2"));
        assert_eq!(before,std::fs::read(&path).unwrap());
    }
    #[test]
    fn batch_packs_without_copy_and_reopens_each_identity() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");
        let mut store=unsafe{Store::open(&path).unwrap()};
        let before=store.metrics();
        let mut addresses=Vec::new();
        let entries=(0..64).map(|id|(id,256)).collect::<Vec<_>>();
        let batch=store.insert_batch_bound(&entries,&mut |i,dst|{addresses.push(dst.as_ptr());dst.fill(b'a'+(i%26) as u8);}).unwrap();
        let after=store.metrics();
        assert_eq!(after.private_commits-before.private_commits,1);
        assert_eq!(after.initialized_page_bytes-before.initialized_page_bytes,5*4096);
        let descriptors=batch.iter().map(|(d,_)|d.clone()).collect::<Vec<_>>();
        for (i,(descriptor,bytes)) in batch.iter().enumerate(){
            assert_eq!(bytes.as_ptr(),addresses[i]);assert_eq!(bytes.len(),256);
            assert_eq!(descriptor.page%4096,0);assert_eq!((descriptor.offset-HEADER)%8,0);
            assert_eq!(bytes[0],b'a'+(i%26) as u8);
        }
        // Last-page slack must never be reopened for later writes.
        let extra=store.insert_batch_bound(&[(100,1)],&mut |_,out|out[0]=b'!').unwrap();
        assert!(!descriptors.iter().any(|d|d.page==extra[0].0.page));
        drop(extra);drop(batch);drop(store);
        let store=unsafe{Store::open(&path).unwrap()};
        for (i,descriptor) in descriptors.iter().enumerate(){
            let bytes=store.bind(descriptor).unwrap();assert_eq!(&*bytes,vec![b'a'+(i%26) as u8;256]);
            let mut wrong=descriptor.clone();wrong.offset+=8;assert!(store.bind(&wrong).is_err());
        }
    }
    #[test]
    fn batch_abort_does_not_publish_partial_extents() {
        let dir=tempfile::tempdir().unwrap();let mut store=unsafe{Store::open(&dir.path().join("data")).unwrap()};
        let retained=store.insert_bound(99,1,&mut |out|out[0]=b'z').unwrap().1;
        let before=store.metrics().private_commits;
        assert!(store.insert_batch_bound(&[(1,1),(2,1)],&mut |i,out|out[0]=if i==0{b'a'}else{255}).is_err());
        assert!(store.get(1).is_err());assert!(store.get(2).is_err());
        assert_eq!(store.metrics().private_commits,before);
        let panic=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
            let _=store.insert_batch_bound(&[(1,1),(2,1)],&mut |i,out|{if i==1{panic!("batch abort")};out[0]=b'a';});
        }));
        assert!(panic.is_err());assert!(store.get(1).is_err());assert!(store.get(2).is_err());
        let mut calls=0;
        assert!(store.insert_batch_bound(&[(1,1),(1,1)],&mut |_,_|calls+=1).is_err());
        assert!(store.insert_batch_bound(&[(1,1),(99,1)],&mut |_,_|calls+=1).is_err());
        assert_eq!(calls,0);assert_eq!(&*retained,b"z");
        let bytes=store.insert_batch_bound(&[(1,4064),(2,3)],&mut |_,out|out.fill(b'x')).unwrap();
        assert_ne!(bytes[0].0.page,bytes[1].0.page);
    }
    #[test]
    fn packed_consumers_keep_mapping_and_lock_after_store_drop() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");
        let mut store=unsafe{Store::open(&path).unwrap()};
        let mut batch=store.insert_batch_bound(&[(1,1),(2,1)],&mut |i,out|out[0]=b'a'+i as u8).unwrap();
        assert_eq!(batch[0].0.page,batch[1].0.page);
        let (_,second)=batch.pop().unwrap();let (_,first)=batch.pop().unwrap();drop(batch);drop(store);
        assert!(unsafe{Store::open(&path)}.is_err());drop(first);
        assert_eq!(&*second,b"b");assert!(unsafe{Store::open(&path)}.is_err());
        std::thread::spawn(move||assert_eq!(&*second,b"b")).join().unwrap();
        assert!(unsafe{Store::open(&path)}.is_ok());
    }
}
