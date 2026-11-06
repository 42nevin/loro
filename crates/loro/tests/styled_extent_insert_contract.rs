#![cfg(feature = "shared_arena_experiment")]
//! Independent contracts for the real style-aware native insertion hook.
use loro::{
    ExpandType, ExportMode, ExtentResolver, JsonOpContent, JsonTextOp, LoroDoc, StyleConfig,
    TextAllocationContext, TextExtentAllocator, TextSlice, VersionVector,
};
use std::{
    collections::BTreeMap,
    io,
    ops::Range,
    sync::{Arc, RwLock},
};
#[derive(Default)]
struct Memory(RwLock<BTreeMap<u64, Vec<u8>>>);
impl ExtentResolver for Memory {
    fn with_bytes(
        &self,
        id: u64,
        range: Range<usize>,
        _: &'static str,
        visit: &mut dyn FnMut(&[u8]),
    ) -> io::Result<()> {
        let guard = self.0.try_read().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "exclusive mutation during immutable extent visit",
            )
        })?;
        let bytes = guard
            .get(&id)
            .and_then(|b| b.get(range))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing extent/range"))?;
        visit(bytes);
        Ok(())
    }
}
struct Allocator {
    memory: Arc<Memory>,
    context: Option<TextAllocationContext>,
    allocations: Vec<(TextAllocationContext, TextSlice)>,
}
impl TextExtentAllocator for Allocator {
    fn begin_allocation(&mut self, context: &TextAllocationContext) -> io::Result<()> {
        self.context = Some(context.clone());
        Ok(())
    }
    fn allocate_text(
        &mut self,
        len: usize,
        generate: &mut dyn FnMut(&mut [u8]),
    ) -> io::Result<TextSlice> {
        let id = self.allocations.len() as u64 + 100;
        let mut bytes = vec![0; len];
        generate(&mut bytes);
        self.memory.0.write().unwrap().insert(id, bytes);
        let slice = TextSlice::extent(self.memory.clone(), id, 0..len);
        self.allocations.push((
            self.context.take().expect("native allocation context"),
            slice.clone(),
        ));
        Ok(slice)
    }
}

fn styled(expand: ExpandType) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(42).unwrap();
    doc.set_record_timestamp(false);
    doc.config_default_text_style(Some(StyleConfig { expand }));
    let text = doc.get_text("styled");
    text.insert(0, "a玉🙂Z").unwrap();
    doc.commit();
    text.mark(1..3, "bold", true).unwrap();
    doc.commit();
    doc
}
fn equivalent(expected: &LoroDoc, actual: &LoroDoc) {
    assert_eq!(expected.get_deep_value(), actual.get_deep_value());
    assert_eq!(
        expected.get_text("styled").get_richtext_value(),
        actual.get_text("styled").get_richtext_value()
    );
    assert_eq!(expected.oplog_vv(), actual.oplog_vv());
    assert_eq!(
        serde_json::to_value(expected.export_json_updates_without_peer_compression(
            &VersionVector::default(),
            &expected.oplog_vv()
        ))
        .unwrap(),
        serde_json::to_value(actual.export_json_updates_without_peer_compression(
            &VersionVector::default(),
            &actual.oplog_vv()
        ))
        .unwrap()
    );
    let wire = actual.export(ExportMode::all_updates()).unwrap();
    let received = LoroDoc::new();
    received.import(&wire).unwrap();
    received.checkout(&actual.state_frontiers()).unwrap();
    assert_eq!(received.get_deep_value(), expected.get_deep_value());
    assert_eq!(received.oplog_vv(), expected.oplog_vv());
}
#[test]
fn styled_extent_insert_matches_ordinary_at_all_unicode_boundaries() {
    for expand in [
        ExpandType::Before,
        ExpandType::After,
        ExpandType::Both,
        ExpandType::None,
    ] {
        for position in 0..=4 {
            let ordinary = styled(expand);
            let owned = styled(expand);
            let cut = owned.state_frontiers();
            let memory = Arc::new(Memory::default());
            let mut allocator = Allocator {
                memory,
                context: None,
                allocations: vec![],
            };
            ordinary
                .get_text("styled")
                .insert(position, "é界🦀")
                .unwrap();
            ordinary.commit();
            owned
                .get_text("styled")
                .insert_unicode_with_allocator(position, "é界🦀", &mut allocator)
                .unwrap();
            owned.commit();
            assert_eq!(allocator.allocations.len(), 1);
            let (context, slice) = &allocator.allocations[0];
            assert_eq!(context.unicode_pos, position);
            assert_eq!(
                slice.try_with_bytes("test", |b| b.to_vec()).unwrap(),
                "é界🦀".as_bytes()
            );
            let schema = owned.export_json_updates_without_peer_compression(
                &VersionVector::default(),
                &owned.oplog_vv(),
            );
            let mut matched = 0;
            for change in &schema.changes {
                for op in &change.ops {
                    if let JsonOpContent::Text(JsonTextOp::Insert { text, pos, .. }) = &op.content {
                        if text == "é界🦀" {
                            matched += 1;
                            assert_eq!(context.id.peer, change.id.peer);
                            assert_eq!(context.id.counter, op.counter);
                            assert_eq!(context.lamport,change.lamport+(op.counter-change.id.counter) as u32);
                            assert_eq!(context.container, op.container);
                            assert_eq!(context.entity_pos, *pos as usize);
                        }
                    }
                }
            }
            assert_eq!(matched, 1);
            equivalent(&ordinary, &owned);
            ordinary.get_text("styled").delete(position, 3).unwrap();
            ordinary.commit();
            owned.get_text("styled").delete(position, 3).unwrap();
            owned.commit();
            equivalent(&ordinary, &owned);
            ordinary.checkout(&cut).unwrap();
            owned.checkout(&cut).unwrap();
            equivalent(&ordinary, &owned);
        }
    }
}
#[test]
fn styled_extent_insert_failure_and_bounds_leave_native_state_unchanged() {
    struct Fail {
        begin: bool,
        calls: usize,
    }
    impl TextExtentAllocator for Fail {
        fn begin_allocation(&mut self, _: &TextAllocationContext) -> io::Result<()> {
            self.calls += 1;
            if self.begin {
                Err(io::Error::other("begin failure"))
            } else {
                Ok(())
            }
        }
        fn allocate_text(
            &mut self,
            _: usize,
            _: &mut dyn FnMut(&mut [u8]),
        ) -> io::Result<TextSlice> {
            self.calls += 1;
            Err(io::Error::other("allocation failure"))
        }
    }
    for begin in [false, true] {
        let doc = styled(ExpandType::Both);
        let expected = styled(ExpandType::Both);
        let mut fail = Fail { begin, calls: 0 };
        assert!(
            doc.get_text("styled")
                .insert_unicode_with_allocator(1, "玉", &mut fail)
                .is_err()
        );
        assert_eq!(fail.calls, if begin { 1 } else { 2 });
        equivalent(&expected, &doc);
        // Failure must leave the next actual op identities and style anchors intact.
        doc.get_text("styled").insert(1, "next").unwrap();
        doc.commit();
        expected.get_text("styled").insert(1, "next").unwrap();
        expected.commit();
        equivalent(&expected, &doc);
    }
    let ordinary = styled(ExpandType::After);
    let owned = styled(ExpandType::After);
    let mut allocator = Allocator {
        memory: Arc::new(Memory::default()),
        context: None,
        allocations: vec![],
    };
    assert!(
        owned
            .get_text("styled")
            .insert_unicode_with_allocator(5, "x", &mut allocator)
            .is_err()
    );
    assert!(allocator.allocations.is_empty());
    equivalent(&ordinary, &owned);
    ordinary.get_text("styled").insert(1, "").unwrap();
    owned
        .get_text("styled")
        .insert_unicode_with_allocator(1, "", &mut allocator)
        .unwrap();
    assert!(allocator.allocations.is_empty());
    equivalent(&ordinary, &owned);
}

#[test]
fn styled_extent_insert_rejects_forged_allocator_results_before_native_adoption() {
    struct Bad(u8, Arc<Memory>);
    impl TextExtentAllocator for Bad {
        fn allocate_text(
            &mut self,
            len: usize,
            generate: &mut dyn FnMut(&mut [u8]),
        ) -> io::Result<TextSlice> {
            let mut bytes = vec![0; len];
            if self.0 != 0 {
                generate(&mut bytes);
            }
            if self.0 == 1 {
                generate(&mut bytes);
            }
            if self.0 == 2 {
                bytes.fill(b'x');
            }
            if self.0 == 3 {
                bytes.fill(0xff);
            }
            if self.0 == 4 {
                bytes.push(b'x');
            }
            let size = bytes.len();
            self.1.0.write().unwrap().insert(1, bytes);
            Ok(TextSlice::extent(self.1.clone(), 1, 0..size))
        }
    }
    for fault in 0..5 {
        let doc = styled(ExpandType::Both);
        let expected = styled(ExpandType::Both);
        let mut allocator = Bad(fault, Arc::new(Memory::default()));
        assert!(
            doc.get_text("styled")
                .insert_unicode_with_allocator(1, "玉", &mut allocator)
                .is_err(),
            "bad allocator {fault}"
        );
        equivalent(&expected, &doc);
        // External failed reservations can remain private; only the native
        // transaction is required to roll back at this provider boundary.
        doc.get_text("styled").insert(1, "ok").unwrap();
        doc.commit();
        expected.get_text("styled").insert(1, "ok").unwrap();
        expected.commit();
        equivalent(&expected, &doc);
    }
}
