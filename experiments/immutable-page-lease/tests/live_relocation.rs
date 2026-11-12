use immutable_page_lease::Store;
#[test]
fn repeated_relocation_preserves_old_owners_and_journal() {
 let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");
 let mut s=unsafe{Store::open(&path).unwrap()};
 let (d,a)=s.insert_bound(1,3,&mut |b|b.copy_from_slice(b"abc")).unwrap();
 let c=s.commitment(&d).unwrap();s.publish(1,b"journal",std::slice::from_ref(&d)).unwrap();
 let d2=s.relocate(&c).unwrap(); let (_,b)=s.bind_committed(&c).unwrap();
 let d3=s.relocate(&c).unwrap();assert_ne!(d.page,d2.page);assert_ne!(d2.page,d3.page);
 assert!(s.bind(&d).is_err());assert!(s.reclaim_relocated().unwrap().is_empty());
 assert_eq!(&*a,b"abc");assert_eq!(&*b,b"abc");
 drop(a);assert_eq!(s.reclaim_relocated().unwrap(),vec![d.page]);
 std::thread::spawn(move || assert_eq!(&*b,b"abc")).join().unwrap();
 assert_eq!(s.reclaim_relocated().unwrap(),vec![d2.page]);
 assert_eq!(s.published(1).unwrap().unwrap(),b"journal");drop(s);
 let s=unsafe{Store::open(&path).unwrap()};let (now,bytes)=s.bind_committed(&c).unwrap();assert_eq!(now,d3);assert_eq!(&*bytes,b"abc");
}
#[test]
fn packed_neighbor_blocks_reclaim_and_corruption_is_detected() {
 let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");
 let mut s=unsafe{Store::open(&path).unwrap()};
 let pair=s.insert_batch_bound(&[(1,3),(2,3)],&mut |_,b|b.copy_from_slice(b"abc")).unwrap();
 let c=s.commitment(&pair[0].0).unwrap();let neighbor=s.commitment(&pair[1].0).unwrap();let old=pair[0].0.page;drop(pair);
 let moved=s.relocate(&c).unwrap();assert!(s.reclaim_relocated().unwrap().is_empty());
 s.relocate(&neighbor).unwrap();assert_eq!(s.reclaim_relocated().unwrap(),vec![old]);drop(s);
 use std::os::unix::fs::FileExt;
 let f=std::fs::OpenOptions::new().write(true).open(&path).unwrap();f.write_all_at(b"xyz",moved.page+moved.offset as u64).unwrap();f.sync_all().unwrap();drop(f);
 let s=unsafe{Store::open(&path).unwrap()};assert!(s.bind_committed(&c).is_err());assert!(s.get(1).is_err());
}
#[test]
fn reclaimed_pages_are_reused_without_changing_logical_content() {
 let dir=tempfile::tempdir().unwrap();let mut s=unsafe{Store::open(&dir.path().join("data")).unwrap()};
 s.init_derived_v1().unwrap();s.derived_put(1,b"derived-object").unwrap();s.derived_publish_head(1,b"head",&[1]).unwrap();
 let (first,b)=s.insert_bound(1,4064,&mut |b|b.fill(b'x')).unwrap();let c=s.commitment(&first).unwrap();drop(b);
 s.publish_committed(1,b"original-journal",std::slice::from_ref(&c)).unwrap();
 let mut reclaimed=std::collections::BTreeSet::new();let mut reused=false;
 for _ in 0..64 {let moved=s.relocate(&c).unwrap();if reclaimed.contains(&moved.page){reused=true;}reclaimed.extend(s.reclaim_relocated().unwrap());assert_eq!(&*s.bind_committed(&c).unwrap().1,vec![b'x';4064]);}
 assert!(reused,"allocator must demonstrably reuse old payload pages");
 assert_eq!(s.published(1).unwrap().unwrap(),b"original-journal");assert_eq!(s.derived_get(1).unwrap().unwrap(),b"derived-object");assert_eq!(s.derived_head().unwrap().unwrap(),(1,b"head".to_vec()));
 s.publish_committed(2,b"new-publication",&[c]).unwrap();
}
#[test]
fn other_thread_lease_blocks_reuse_and_keeps_store_exclusion() {
 let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");let mut s=unsafe{Store::open(&path).unwrap()};
 let (d,b)=s.insert_bound(1,3,&mut |b|b.copy_from_slice(b"abc")).unwrap();let c=s.commitment(&d).unwrap();
 let (release_tx,release_rx)=std::sync::mpsc::channel();let (ready_tx,ready_rx)=std::sync::mpsc::channel();
 let reader=std::thread::spawn(move || {ready_tx.send(()).unwrap();release_rx.recv().unwrap();assert_eq!(&*b,b"abc");});
 ready_rx.recv().unwrap();s.relocate(&c).unwrap();assert!(s.reclaim_relocated().unwrap().is_empty());drop(s);
 assert!(unsafe{Store::open(&path)}.is_err());release_tx.send(()).unwrap();reader.join().unwrap();
 let mut s=unsafe{Store::open(&path).unwrap()};assert_eq!(s.reclaim_relocated().unwrap(),vec![d.page]);assert_eq!(&*s.bind_committed(&c).unwrap().1,b"abc");
}
