//! Explicit optional schema, distinct from canonical public-record IDs.
//! All objects and head blobs are immutable; no garbage collection or upgrades.
use super::*;
const SCHEMA_KEY:u64=2;
const SCHEMA_MAGIC_KEY:u64=3;
const SCHEMA_MAGIC:u64=0x4452564453434831;
const OBJECT_ROOT:usize=3;
const HEAD_ROOT:usize=4;
const OBJECT_MAGIC:u64=0x445256444f424a31;
const HEAD_MAGIC:u64=0x4452564448454431;
impl Store {
    /// Initialize only before any canonical record or payload allocation.
    pub fn init_derived_v1(&mut self)->Result<()> {
        {
            let txn=sanakirja::Env::txn_begin(&self.owner.env)?;
            ensure!(txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(0).is_none(),"derived schema requires pristine payload store");
            ensure!(txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(1).is_none(),"derived schema requires pristine journal");
            let db=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(2).ok_or_else(||anyhow::anyhow!("missing identity"))?;
            ensure!(sanakirja::btree::get(&txn,&db,&SCHEMA_KEY,None)?.is_none_or(|(k,_)|*k!=SCHEMA_KEY),"derived schema already initialized");
        }
        let mut txn=sanakirja::Env::mut_txn_begin(&self.owner.env)?;
        let mut db=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(2).ok_or_else(||anyhow::anyhow!("missing identity"))?;
        sanakirja::btree::put(&mut txn,&mut db,&SCHEMA_KEY,&1)?;
        sanakirja::btree::put(&mut txn,&mut db,&SCHEMA_MAGIC_KEY,&SCHEMA_MAGIC)?;
        txn.set_root(2,db.db.into());txn.commit()?;Ok(())
    }
    /// Read-only admission; legacy/unknown schemas are never upgraded.
    pub fn require_derived_v1(&self)->Result<()> {
        self.owner.counts.reads.fetch_add(1,Ordering::SeqCst);
        let txn=sanakirja::Env::txn_begin(&self.owner.env)?;
        let db=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(2).ok_or_else(||anyhow::anyhow!("missing identity"))?;
        for (key,wanted) in [(SCHEMA_KEY,1),(SCHEMA_MAGIC_KEY,SCHEMA_MAGIC)] {
            let (found,value)=sanakirja::btree::get(&txn,&db,&key,None)?.ok_or_else(||anyhow::anyhow!("missing derived schema v1"))?;
            ensure!(*found==key && *value==wanted,"incompatible derived schema");
        }Ok(())
    }
    pub fn derived_put(&mut self,id:u64,bytes:&[u8])->Result<()> {
        self.require_derived_v1()?;
        self.publish_record(OBJECT_ROOT,OBJECT_MAGIC,id,bytes)
    }
    pub fn derived_get(&self,id:u64)->Result<Option<Vec<u8>>> {
        self.require_derived_v1()?;
        self.read_record(OBJECT_ROOT,OBJECT_MAGIC,id)
    }
    pub fn derived_head(&self)->Result<Option<(u64,Vec<u8>)>> {
        self.require_derived_v1()?;
        let id={
            self.owner.counts.reads.fetch_add(1,Ordering::SeqCst);
            let txn=sanakirja::Env::txn_begin(&self.owner.env)?;
            let Some(db)=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(HEAD_ROOT) else {return Ok(None)};
            let mut iter=sanakirja::btree::rev_iter(&txn,&db,None)?;
            match iter.next() {Some(entry)=>Some(*entry?.0),None=>None}
        };
        match id {Some(id)=>Ok(Some((id,self.read_record(HEAD_ROOT,HEAD_MAGIC,id)?.ok_or_else(||anyhow::anyhow!("missing selected derived head"))?))),None=>Ok(None)}
    }
    /// One exclusive Store writer checks dependencies and commits one new head.
    /// Prior head/object bytes stay immutable for existing readers.
    pub fn derived_publish_head(&mut self,sequence:u64,bytes:&[u8],required:&[u64])->Result<()> {
        self.require_derived_v1()?;
        if let Some((previous,_))=self.derived_head()? {ensure!(sequence>previous,"stale derived head");}
        {
            self.owner.counts.reads.fetch_add(1,Ordering::SeqCst);
            let txn=sanakirja::Env::txn_begin(&self.owner.env)?;
            if !required.is_empty() {
                let db=txn.root_db::<u64,u64,sanakirja::btree::page::Page<u64,u64>>(OBJECT_ROOT).ok_or_else(||anyhow::anyhow!("missing derived objects"))?;
                for id in required {
                    let (found,_)=sanakirja::btree::get(&txn,&db,id,None)?.ok_or_else(||anyhow::anyhow!("missing derived object"))?;
                    ensure!(found==id,"missing derived object");
                }
            }
        }
        self.publish_record(HEAD_ROOT,HEAD_MAGIC,sequence,bytes)
    }
}
