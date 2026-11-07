//! Independent negative/lifetime checks; application acceptance is tested separately.
use immutable_page_lease::Store;

#[test]
fn independent_private_visibility_descriptor_rejection_and_lease_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    // SAFETY: This test owns the private directory and all mappings.
    let mut store = unsafe { Store::open(&path).unwrap() };
    let mut generated = 0usize;
    let (d, owner) = store.insert_bound(10, 3, &mut |out| {
        generated = out.as_ptr() as usize;
        out.copy_from_slice("玉".as_bytes());
    }).unwrap();
    assert_eq!(owner.as_ptr() as usize, generated);
    assert!(store.published_ids().unwrap().is_empty());
    assert_eq!(store.metrics().private_commits, 1);
    assert_eq!(store.metrics().public_commits, 0);
    for kind in 0..4 {
        let mut bad = d.clone();
        match kind { 0 => bad.id += 1, 1 => bad.page += 4096, 2 => bad.len += 1, _ => bad.store_id[0] ^= 1 }
        assert!(store.publish(1, b"metadata", &[bad]).is_err());
        assert!(store.published(1).unwrap().is_none());
    }
    store.publish(1, b"metadata", &[d.clone()]).unwrap();
    assert_eq!(store.published(1).unwrap().unwrap(), b"metadata");
    assert!(store.publish(1, b"different", &[d.clone()]).is_err());
    assert_eq!(store.published(1).unwrap().unwrap(), b"metadata");
    let bound = store.bind(&d).unwrap();
    assert_eq!(bound.as_ptr(), owner.as_ptr());
    let before = store.metrics();
    for _ in 0..1000 { std::hint::black_box(&*owner); assert_eq!(&*bound, "玉".as_bytes()); }
    assert_eq!(store.metrics(), before, "direct reads must not open transactions or resolve IDs");
    drop(store);
    let alias = dir.path().join("hardlink-alias");
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(unsafe { Store::open(&alias) }.is_err(), "inode lock must reject alternate path");
    // A failed lock acquisition never maps a second Env, so testing exclusion is safe.
    assert!(unsafe { Store::open(&path) }.is_err());
    drop(owner);
    assert!(unsafe { Store::open(&path) }.is_err());
    assert_eq!(&*bound, "玉".as_bytes());
    drop(bound);
    let store = unsafe { Store::open(&path).unwrap() };
    assert_eq!(store.published(1).unwrap().unwrap(), b"metadata");
    assert_eq!(&*store.bind(&d).unwrap(), "玉".as_bytes());
    println!("independent_store private_only_visible=false missing_conflicting_rejected=4 direct_reads=1000 lookup_delta=0 transaction_delta=0 lease_lock_survives_facade=true");
}

#[test]
fn independent_metadata_multiple_pages_and_empty_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut store = unsafe { Store::open(&path).unwrap() };
    let payload: Vec<_> = (0..20000).map(|i| (i % 251) as u8).collect();
    store.publish(1, &payload, &[]).unwrap();
    store.publish(2, &[], &[]).unwrap();
    assert_eq!(store.published(1).unwrap().unwrap(), payload);
    assert_eq!(store.published(2).unwrap().unwrap(), Vec::<u8>::new());
    drop(store);
    let store = unsafe { Store::open(&path).unwrap() };
    assert_eq!(store.published_ids().unwrap(), vec![1,2]);
    assert_eq!(store.published(1).unwrap().unwrap(), payload);
}
