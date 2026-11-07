//! DRAFT outside provider tests/: API-red not yet run, no auto-discovery.
use immutable_page_lease::Store;
#[test]
fn derived_keyspace_does_not_pollute_dense_canonical_ids() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("store");
    let mut store=unsafe {Store::open(&path).unwrap()};
    store.init_derived_v1().unwrap();
    store.publish(0,b"canonical0",&[]).unwrap();
    store.derived_put(41,b"target-body").unwrap();store.derived_put(42,b"unrelated-body").unwrap();
    store.derived_publish_head(1,b"head1",&[41,42]).unwrap();
    assert_eq!(store.published_ids().unwrap(),vec![0]);
    assert_eq!(store.published(41).unwrap(),None);
    assert_eq!(store.derived_get(41).unwrap(),Some(b"target-body".to_vec()));
    assert_eq!(store.derived_head().unwrap(),Some((1,b"head1".to_vec())));
    drop(store);
    let store=unsafe {Store::open(&path).unwrap()};
    store.require_derived_v1().unwrap();
    assert_eq!(store.published_ids().unwrap(),vec![0]);
    assert_eq!(store.derived_get(42).unwrap(),Some(b"unrelated-body".to_vec()));
}
#[test]
fn incomplete_or_stale_derived_head_cannot_replace_committed_head() {
    let dir=tempfile::tempdir().unwrap();let mut store=unsafe {Store::open(&dir.path().join("store")).unwrap()};
    store.init_derived_v1().unwrap();store.derived_put(1,b"old").unwrap();
    store.derived_publish_head(7,b"old-head",&[1]).unwrap();
    store.derived_put(2,b"orphan-new-object").unwrap();
    assert!(store.derived_publish_head(8,b"incomplete",&[2,99]).is_err());
    assert!(store.derived_publish_head(6,b"stale",&[2]).is_err());
    assert!(store.derived_put(1,b"overwrite").is_err());
    assert_eq!(store.derived_head().unwrap(),Some((7,b"old-head".to_vec())));
    assert_eq!(store.derived_get(1).unwrap(),Some(b"old".to_vec()));
}
#[test]
fn derived_schema_refuses_legacy_without_mutating_file() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("store");
    let mut store=unsafe {Store::open(&path).unwrap()};store.publish(0,b"old",&[]).unwrap();drop(store);
    let before=std::fs::read(&path).unwrap();
    let store=unsafe {Store::open(&path).unwrap()};assert!(store.require_derived_v1().is_err());drop(store);
    assert_eq!(before,std::fs::read(&path).unwrap());
}
