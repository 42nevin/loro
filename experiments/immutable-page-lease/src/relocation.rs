//! Live relocation: root 6 stores immutable (id, field) content commitments;
//! root 7 stores old payload pages awaiting directory/lease exclusion.
use super::*;
impl Store {
    pub(super) fn require_healthy(&self) -> Result<()> {
        ensure!(!self.failed.load(Ordering::SeqCst), "Store requires reopen after uncertain commit");
        Ok(())
    }
    pub(super) fn commit_result<E: std::error::Error + Send + Sync + 'static>(&self, result: std::result::Result<(), E>) -> Result<()> {
        if let Err(error) = result {
            self.failed.store(true, Ordering::SeqCst);
            return Err(error.into());
        }
        Ok(())
    }
    pub(super) fn read_commitment(&self, id: u64) -> Result<Commitment> {
        self.require_healthy()?;
        self.owner.counts.reads.fetch_add(1, Ordering::SeqCst);
        let txn = sanakirja::Env::txn_begin(&self.owner.env)?;
        let db: sanakirja::btree::Db<CommitmentKey,u64> = txn.root_db(6).ok_or_else(||anyhow::anyhow!("missing commitments"))?;
        let mut words = [0u64; 5];
        for (field, word) in words.iter_mut().enumerate() {
            let key = CommitmentKey(id,field as u64);
            let (found,value) = sanakirja::btree::get(&txn,&db,&key,None)?.ok_or_else(||anyhow::anyhow!("missing commitment"))?;
            ensure!(*found==key,"missing commitment"); *word = *value;
        }
        let mut hash=[0;32];
        for i in 0..4 { hash[i*8..(i+1)*8].copy_from_slice(&words[i+1].to_le_bytes()); }
        Ok(Commitment {store_id:self.store_id,id,len:usize::try_from(words[0])?,hash})
    }
    pub fn commitment(&self, descriptor: &Descriptor) -> Result<Commitment> {
        self.bind(descriptor)?;
        self.read_commitment(descriptor.id)
    }
    pub fn bind_committed(&self, expected: &Commitment) -> Result<(Descriptor,BytesSlice)> {
        ensure!(expected.store_id==self.store_id,"wrong store identity");
        ensure!(self.read_commitment(expected.id)?==*expected,"conflicting content commitment");
        self.resolve(expected.id)
    }
    pub fn publish_committed(&mut self, record_id: u64, metadata: &[u8], extents: &[Commitment]) -> Result<()> {
        self.require_healthy()?;
        for commitment in extents { self.bind_committed(commitment)?; }
        self.publish_record(1, RECORD_MAGIC, record_id, metadata)
    }
    /// Copy one immutable extent and atomically switch its directory entry.
    /// Public and derived records and content identity remain untouched.
    pub fn relocate(&mut self, expected: &Commitment) -> Result<Descriptor> {
        self.require_healthy()?;
        let (old, bytes)=self.bind_committed(expected)?;
        // This owner is kept alive through publication. Capture the weak before
        // invalidating the current-owner shortcut, including every generation.
        let lease=self.live_segments.lock().unwrap().get(&old.id).cloned().expect("resolved lease");
        let mut txn=sanakirja::Env::mut_txn_begin(&self.owner.env)?;
        let mut directory: sanakirja::btree::Db<u64,u64>=txn.root_db(0).ok_or_else(||anyhow::anyhow!("missing directory"))?;
        let mut retired: sanakirja::btree::Db<u64,u64>=match txn.root_db(7) {
            Some(db)=>db,None=>unsafe{sanakirja::btree::create_db(&mut txn)?}
        };
        let page=unsafe{txn.alloc_page()?};
        let raw=unsafe{std::slice::from_raw_parts_mut(page.0.data,PAGE)};
        raw.fill(0);
        raw[8..16].copy_from_slice(&MAGIC.to_le_bytes());
        raw[16..24].copy_from_slice(&(old.len as u64).to_le_bytes());
        raw[24..32].copy_from_slice(&old.id.to_le_bytes());
        raw[HEADER..HEADER+old.len].copy_from_slice(&bytes);
        self.owner.counts.initialized.fetch_add(PAGE as u64,Ordering::SeqCst);
        sanakirja::btree::del(&mut txn,&mut directory,&old.id,None)?;
        sanakirja::btree::put(&mut txn,&mut directory,&old.id,&page.0.offset)?;
        sanakirja::btree::put(&mut txn,&mut retired,&old.page,&0)?;
        txn.set_root(0,directory.db.into());txn.set_root(7,retired.db.into());
        let new=Descriptor {page:page.0.offset,offset:HEADER,..old.clone()};
        #[cfg(test)] self.relocation_test_barrier(3);
        // Deterministic transaction-boundary simulation, NOT power-loss proof.
        #[cfg(test)] if self.relocation_fault==1 {
            self.failed.store(true,Ordering::SeqCst);
            anyhow::bail!("injected abort before publication (not power loss)");
        }
        self.commit_result(txn.commit())?;
        #[cfg(test)] self.relocation_test_barrier(4);
        #[cfg(test)] if self.relocation_fault==2 {
            self.failed.store(true,Ordering::SeqCst);
            anyhow::bail!("injected lost acknowledgment (not power loss)");
        }
        self.retired_segments.push((old.page,lease));
        self.live_segments.lock().unwrap().remove(&old.id);
        self.owner.counts.private.fetch_add(1,Ordering::SeqCst);
        Ok(new)
    }
    #[cfg(test)]
    fn relocation_test_barrier(&self, phase: u8) {
        if self.relocation_fault == phase {
            // Only subprocess tests select 3/4. Parent observes the marker and
            // SIGKILLs us, without transaction/Store destructors running.
            let marker=std::env::var_os("RELOCATION_BARRIER").expect("test barrier marker");
            std::fs::write(marker,b"ready").unwrap();
            loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
        }
    }
    /// Reclaim only relocated physical pages. Logical IDs and commitments,
    /// publication records and derived objects are never garbage-collected.
    pub fn reclaim_relocated(&mut self) -> Result<Vec<u64>> {
        self.require_healthy()?;
        self.retired_segments.retain(|(_,lease)|lease.strong_count()>0);
        let mut pinned=std::collections::BTreeSet::new();
        for (page,_) in &self.retired_segments { pinned.insert(*page); }
        let eligible={
            self.owner.counts.reads.fetch_add(1, Ordering::SeqCst);
            let txn=sanakirja::Env::txn_begin(&self.owner.env)?;
            let Some(retired)=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(7) else{return Ok(Vec::new())};
            if let Some(db)=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(0) {
                for entry in sanakirja::btree::iter(&txn,&db,None)? {let (_,location)=entry?;pinned.insert(*location & !((PAGE as u64)-1));}
            }
            let mut pages=Vec::new();
            for entry in sanakirja::btree::iter(&txn,&retired,None)? {let (page,_)=entry?;if !pinned.contains(page){pages.push(*page);}}
            pages
        };
        if eligible.is_empty(){return Ok(eligible)}
        let mut txn=sanakirja::Env::mut_txn_begin(&self.owner.env)?;
        let mut retired: sanakirja::btree::Db<u64,u64>=txn.root_db(7).unwrap();
        for page in &eligible {
            ensure!(unsafe{txn.decr_rc(*page)?}==0,"unexpected retired payload RC");
            sanakirja::btree::del(&mut txn,&mut retired,page,None)?;
        }
        txn.set_root(7,retired.db.into());
        self.commit_result(txn.commit())?;
        self.owner.counts.private.fetch_add(1,Ordering::SeqCst);
        Ok(eligible)
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test]
    fn relocation_does_not_edit_journal_blob_pages_and_legacy_sweep_refuses() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");
        let mut s=unsafe{Store::open(&path).unwrap()};
        let (d,b)=s.insert_bound(1,3,&mut |out|out.copy_from_slice(b"abc")).unwrap();
        let c=s.commitment(&d).unwrap();drop(b);
        s.publish_committed(1,b"journal bytes",std::slice::from_ref(&c)).unwrap();
        let (offset,before)={
            let txn=sanakirja::Env::txn_begin(&s.owner.env).unwrap();
            let db: sanakirja::btree::Db<u64,u64>=txn.root_db(1).unwrap();
            let offset=*sanakirja::btree::get(&txn,&db,&1,None).unwrap().unwrap().1;
            let page=unsafe{txn.load_page(offset).unwrap()};
            (offset,unsafe{std::slice::from_raw_parts(page.data,PAGE)}.to_vec())
        };
        s.relocate(&c).unwrap();s.reclaim_relocated().unwrap();
        let txn=sanakirja::Env::txn_begin(&s.owner.env).unwrap();
        let page=unsafe{txn.load_page(offset).unwrap()};
        assert_eq!(before,unsafe{std::slice::from_raw_parts(page.data,PAGE)});drop(txn);
        // Use a second store without journal records, so root 7 is the refusal.
        let mut s=unsafe{Store::open(&dir.path().join("bare")).unwrap()};
        let (d,b)=s.insert_bound(1,3,&mut |out|out.copy_from_slice(b"abc")).unwrap();
        let c=s.commitment(&d).unwrap();s.relocate(&c).unwrap();
        assert!(super::super::reclamation_probe::sweep_with_manifest(&mut s,&Default::default(),None).is_err());
        assert_eq!(&*b,b"abc");
    }
    #[test]
    fn abrupt_exit_child() {
        let Some(path)=std::env::var_os("RELOCATION_CHILD_STORE") else {return};
        let mut s=unsafe{Store::open(Path::new(&path)).unwrap()};
        let c=s.commitment(&s.descriptor(1).unwrap()).unwrap();
        s.relocation_fault=std::env::var("RELOCATION_CHILD_PHASE").unwrap().parse().unwrap();
        s.relocate(&c).unwrap();
        panic!("parent must kill at barrier");
    }
    #[test]
    fn sigkill_before_and_after_publication_recovers_mapping_and_queue() {
        use std::process::{Command,Stdio};
        for phase in [3,4] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");let marker=dir.path().join("barrier");
            let mut s=unsafe{Store::open(&path).unwrap()};
            let (old,b)=s.insert_bound(1,3,&mut |b|b.copy_from_slice(b"abc")).unwrap();
            let c=s.commitment(&old).unwrap();s.publish_committed(1,b"journal",std::slice::from_ref(&c)).unwrap();drop(b);drop(s);
            let mut child=Command::new(std::env::current_exe().unwrap())
                .args(["--exact","store::relocation::tests::abrupt_exit_child","--nocapture"])
                .env("RELOCATION_CHILD_STORE",&path).env("RELOCATION_CHILD_PHASE",phase.to_string())
                .env("RELOCATION_BARRIER",&marker).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(20);
            while !marker.exists() {
                if let Some(status)=child.try_wait().unwrap(){panic!("barrier child exited early: {status}");}
                if std::time::Instant::now()>=deadline {child.kill().unwrap();child.wait().unwrap();panic!("barrier timeout");}
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            child.kill().unwrap();let status=child.wait().unwrap();
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(),Some(libc::SIGKILL));
            let mut s=unsafe{Store::open(&path).unwrap()};let (now,b)=s.bind_committed(&c).unwrap();
            assert_eq!(now==old,phase==3);assert_eq!(&*b,b"abc");
            assert_eq!(s.published(1).unwrap().unwrap(),b"journal");
            assert_eq!(s.reclaim_relocated().unwrap(),if phase==4{vec![old.page]}else{vec![]});
        }
    }
    #[test]
    fn simulated_abort_and_lost_ack_require_reopen() {
        for fault in [1,2] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");
            let mut s=unsafe{Store::open(&path).unwrap()};
            let (old,bytes)=s.insert_bound(1,3,&mut |b|b.copy_from_slice(b"abc")).unwrap();
            let c=s.commitment(&old).unwrap();s.publish(1,b"unchanged",std::slice::from_ref(&old)).unwrap();
            s.relocation_fault=fault;assert!(s.relocate(&c).is_err());
            assert!(s.bind_committed(&c).is_err());assert!(s.reclaim_relocated().is_err());
            assert!(s.insert_bound(2,1,&mut |b|b[0]=b'x').is_err());
            assert!(s.publish(2,b"blocked",&[]).is_err());assert_eq!(&*bytes,b"abc");
            drop(bytes);drop(s);
            let mut s=unsafe{Store::open(&path).unwrap()};let (now,b)=s.bind_committed(&c).unwrap();
            assert_eq!(now==old,fault==1);assert_eq!(&*b,b"abc");
            assert_eq!(s.published(1).unwrap().unwrap(),b"unchanged");
            let reclaimed=s.reclaim_relocated().unwrap();assert_eq!(reclaimed,if fault==2{vec![old.page]}else{vec![]});
        }
    }
}
