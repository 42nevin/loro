#![cfg(feature = "shared_arena_experiment")]
use loro::{
    ContainerID, ContainerType, ExportMode, ExtentChangeDescriptor, ExtentOpDescriptor,
    ExtentTextOp, Frontiers, JsonOpContent, JsonTextOp, LoroDoc, TextSlice, VersionVector, ID,
};

// JSON is a test oracle only. Production restore consumes the typed descriptors
// directly, with logical TextSlice handles supplied by the database adapter.
fn descriptors(doc: &LoroDoc) -> Vec<ExtentChangeDescriptor> {
    doc.export_json_updates_without_peer_compression(&VersionVector::default(), &doc.oplog_vv())
        .changes
        .into_iter()
        .map(|c| ExtentChangeDescriptor {
            id: c.id,
            lamport: c.lamport,
            timestamp: c.timestamp,
            deps: Frontiers::from_iter(c.deps),
            message: c.msg,
            ops: c
                .ops
                .into_iter()
                .map(|o| ExtentOpDescriptor {
                    counter: o.counter,
                    container: o.container,
                    content: match o.content {
                        JsonOpContent::Text(JsonTextOp::Insert { pos, text }) => {
                            ExtentTextOp::Insert {
                                pos,
                                text: TextSlice::from_bytes(text.as_bytes()),
                            }
                        }
                        JsonOpContent::Text(JsonTextOp::Delete { pos, len, start_id }) => {
                            ExtentTextOp::Delete { pos, len, start_id }
                        }
                        _ => panic!("test fixture must contain only text insert/delete"),
                    },
                })
                .collect(),
        })
        .collect()
}

#[test]
fn typed_restore_preserves_concurrent_ids_cuts_deletes_and_native_sync() {
    let source = LoroDoc::new();
    source.set_peer_id(41).unwrap();
    source.get_text("body").insert(0, "a玉🙂b").unwrap();
    source.commit();
    let cut = source.oplog_frontiers();
    let left = source.fork_cow().unwrap();
    left.set_peer_id(42).unwrap();
    let right = source.fork_cow().unwrap();
    right.set_peer_id(43).unwrap();
    left.get_text("body").insert(1, "左").unwrap();
    left.commit();
    right.get_text("body").insert(1, "右").unwrap();
    right.commit();
    source
        .import(&left.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    source
        .import(&right.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    source.get_text("body").delete(2, 1).unwrap();
    source.commit();
    let restored = LoroDoc::new();
    let status = restored
        .import_extent_descriptors(descriptors(&source))
        .unwrap();
    assert!(status.pending.is_none());
    assert_eq!(restored.oplog_vv(), source.oplog_vv());
    assert_eq!(restored.oplog_frontiers(), source.oplog_frontiers());
    assert_eq!(restored.get_deep_value(), source.get_deep_value());
    restored.checkout(&cut).unwrap();
    assert_eq!(restored.get_text("body").to_string(), "a玉🙂b");
    restored.checkout_to_latest();
    restored.set_peer_id(44).unwrap();
    restored.get_text("body").insert(1, "新").unwrap();
    restored.commit();
    let peer = LoroDoc::new();
    peer.import(&restored.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    assert_eq!(peer.get_deep_value(), restored.get_deep_value());
    assert_eq!(peer.oplog_vv(), restored.oplog_vv());
}

#[test]
fn invalid_later_descriptor_rolls_back_valid_prefix_and_arena() {
    let good = ExtentChangeDescriptor {
        id: ID::new(55, 0),
        lamport: 0,
        timestamp: 0,
        deps: Frontiers::default(),
        message: None,
        ops: vec![ExtentOpDescriptor {
            counter: 0,
            container: ContainerID::new_root("body", ContainerType::Text),
            content: ExtentTextOp::Insert {
                pos: 0,
                text: TextSlice::from_bytes("玉".as_bytes()),
            },
        }],
    };
    let mut bad = good.clone();
    bad.id = ID::new(55, 1);
    bad.ops[0].counter = 2;
    let doc = LoroDoc::new();
    assert!(doc
        .import_extent_descriptors(vec![good.clone(), bad])
        .is_err());
    assert_eq!(doc.oplog_vv(), VersionVector::default());
    assert_eq!(doc.get_text("body").to_string(), "");
    doc.import_extent_descriptors(vec![good]).unwrap();
    assert_eq!(doc.get_text("body").to_string(), "玉");
    assert_eq!(doc.oplog_vv().get(&55), Some(&1));
}

#[test]
fn missing_dependency_remains_pending_until_original_parent_arrives() {
    let source = LoroDoc::new();
    source.set_peer_id(66).unwrap();
    source.get_text("body").insert(0, "a").unwrap();
    source.commit();
    let first = descriptors(&source);
    let child = source.fork_cow().unwrap();
    child.set_peer_id(67).unwrap();
    child.get_text("body").insert(1, "玉").unwrap();
    child.commit();
    let second = descriptors(&child)
        .into_iter()
        .filter(|c| c.id.peer == 67)
        .collect();
    let restored = LoroDoc::new();
    assert!(restored
        .import_extent_descriptors(second)
        .unwrap()
        .pending
        .is_some());
    assert_eq!(restored.get_text("body").to_string(), "");
    restored.import_extent_descriptors(first).unwrap();
    assert_eq!(restored.get_text("body").to_string(), "a玉");
    assert_eq!(restored.oplog_vv(), child.oplog_vv());
}

#[test]
fn wrong_supplied_lamport_is_rejected_before_history_publication() {
    let source = LoroDoc::new();
    source.set_peer_id(70).unwrap();
    source.get_text("body").insert(0, "a").unwrap();
    source.commit();
    let mut changes = descriptors(&source);
    changes[0].lamport += 1;
    let doc = LoroDoc::new();
    assert!(doc.import_extent_descriptors(changes).is_err());
    assert_eq!(doc.oplog_vv(), VersionVector::default());
}

#[test]
fn logical_restore_keeps_handles_after_wire_export_and_middle_edit() {
    use loro::ExtentResolver;
    use std::{
        io,
        ops::Range,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };
    struct Resolver {
        payload: Vec<u8>,
        reads: Mutex<Vec<(&'static str, usize)>>,
        materialized: AtomicUsize,
    }
    impl ExtentResolver for Resolver {
        fn with_bytes(
            &self,
            extent: u64,
            range: Range<usize>,
            consumer: &'static str,
            visit: &mut dyn FnMut(&[u8]),
        ) -> io::Result<()> {
            assert_eq!(extent, 901);
            let bytes = self
                .payload
                .get(range)
                .ok_or_else(|| io::Error::other("bad extent range"))?;
            self.reads.lock().unwrap().push((consumer, bytes.len()));
            visit(bytes);
            Ok(())
        }
        fn record_materialization(&self, _: u64, _: Range<usize>, _: &'static str, bytes: usize) {
            self.materialized.fetch_add(bytes, Ordering::Relaxed);
        }
    }
    let resolver = Arc::new(Resolver {
        payload: "a玉🙂b".as_bytes().to_vec(),
        reads: Mutex::new(Vec::new()),
        materialized: AtomicUsize::new(0),
    });
    let doc = LoroDoc::new();
    let change = ExtentChangeDescriptor {
        id: ID::new(71, 0),
        lamport: 0,
        timestamp: 0,
        deps: Frontiers::default(),
        message: None,
        ops: vec![ExtentOpDescriptor {
            counter: 0,
            container: ContainerID::new_root("body", ContainerType::Text),
            content: ExtentTextOp::Insert {
                pos: 0,
                text: TextSlice::extent(resolver.clone(), 901, 0..resolver.payload.len()),
            },
        }],
    };
    doc.import_extent_descriptors(vec![change]).unwrap();
    assert_eq!(
        resolver.materialized.load(Ordering::Relaxed),
        0,
        "restore should adopt handles and use scoped views, not materialize text"
    );
    resolver.reads.lock().unwrap().clear();
    let initial_backings = doc.inspect_text_op_backings();
    assert_eq!(initial_backings.len(), 1);
    assert_eq!(initial_backings[0].kind, "extent");
    assert_eq!(initial_backings[0].extent, Some(901));
    assert_eq!(initial_backings[0].range, 0..resolver.payload.len());
    assert!(
        resolver.reads.lock().unwrap().is_empty(),
        "backing inspection must not read payloads"
    );
    assert_eq!(doc.get_text("body").to_string(), "a玉🙂b");
    assert!(!resolver.reads.lock().unwrap().is_empty());
    let first_wire = doc.export(ExportMode::all_updates()).unwrap();
    resolver.reads.lock().unwrap().clear();
    let exported_backings = doc.inspect_text_op_backings();
    assert_eq!(exported_backings.len(), 1);
    assert_eq!(exported_backings[0].kind, "extent");
    assert_eq!(exported_backings[0].extent, Some(901));
    assert_eq!(exported_backings[0].range, 0..resolver.payload.len());
    assert!(
        resolver.reads.lock().unwrap().is_empty(),
        "post-export inspection must not resolve bytes"
    );
    let peer = LoroDoc::new();
    peer.import(&first_wire).unwrap();
    assert_eq!(peer.get_text("body").to_string(), "a玉🙂b");
    resolver.reads.lock().unwrap().clear();
    doc.set_peer_id(72).unwrap();
    doc.get_text("body").insert(2, "中").unwrap();
    doc.commit();
    assert!(
        !resolver.reads.lock().unwrap().is_empty(),
        "middle edit must resolve old extent bytes"
    );
    let full = doc.export(ExportMode::all_updates()).unwrap();
    peer.import(&full).unwrap();
    assert_eq!(doc.get_text("body").to_string(), "a玉中🙂b");
    assert_eq!(peer.get_deep_value(), doc.get_deep_value());
    assert_eq!(peer.oplog_vv(), doc.oplog_vv());
}

#[test]
fn failed_resolution_aborts_restore_and_poisoned_candidate_cleanup_is_isolated() {
    use loro::ExtentResolver;
    use std::{
        io,
        ops::Range,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    struct Resolver {
        payload: Vec<u8>,
        fail_next: AtomicBool,
    }
    impl ExtentResolver for Resolver {
        fn with_bytes(
            &self,
            _: u64,
            range: Range<usize>,
            _: &'static str,
            visit: &mut dyn FnMut(&[u8]),
        ) -> io::Result<()> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected extent read failure"));
            }
            visit(&self.payload[range]);
            Ok(())
        }
    }
    let resolver = Arc::new(Resolver {
        payload: "a玉🙂b".as_bytes().to_vec(),
        fail_next: AtomicBool::new(true),
    });
    let descriptor = || ExtentChangeDescriptor {
        id: ID::new(81, 0),
        lamport: 0,
        timestamp: 0,
        deps: Frontiers::default(),
        message: None,
        ops: vec![ExtentOpDescriptor {
            counter: 0,
            container: ContainerID::new_root("body", ContainerType::Text),
            content: ExtentTextOp::Insert {
                pos: 0,
                text: TextSlice::extent(resolver.clone(), 1, 0..resolver.payload.len()),
            },
        }],
    };
    let source = LoroDoc::new();
    assert!(source
        .import_extent_descriptors(vec![descriptor()])
        .is_err());
    assert_eq!(source.oplog_vv(), VersionVector::default());
    assert_eq!(source.get_text("body").to_string(), "");
    source
        .import_extent_descriptors(vec![descriptor()])
        .unwrap();
    let original_vv = source.oplog_vv();
    let original_value = source.get_deep_value();
    let candidate = source.fork_cow().unwrap();
    candidate.set_peer_id(82).unwrap();
    resolver.fail_next.store(true, Ordering::SeqCst);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        candidate.get_text("body").insert(2, "中").unwrap();
    }));
    assert!(
        failed.is_err(),
        "legacy infallible read must surface the injected failure"
    );
    // The candidate is deliberately never exported or published. Failure of an
    // infallible legacy consumer is recoverable only by abandoning this branch.
    resolver.fail_next.store(false, Ordering::SeqCst);
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(candidate)));
    assert!(
        cleanup.is_err(),
        "known qualification limit: poisoned candidate Drop also panics"
    );
    assert_eq!(source.oplog_vv(), original_vv);
    assert_eq!(source.get_deep_value(), original_value);
    source.set_peer_id(83).unwrap();
    source.get_text("body").insert(2, "好").unwrap();
    source.commit();
    assert_eq!(source.get_text("body").to_string(), "a玉好🙂b");
}
