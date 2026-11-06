#![cfg(feature = "shared_arena_experiment")]
//! Provider-only acceptance: real mixed JSON structure/style plus original extents.
use loro::{
    native_wire_recipe::{BoundTextExtents, ExtentProvenance, NativeWireLedger},
    ContainerID, ExpandType, ExportMode, ExtentResolver, JsonOpContent, JsonSchema, JsonTextOp,
    LoroDoc, LoroError, LoroList, LoroResult, LoroText, StyleConfig, TextAllocationContext,
    TextExtentAllocator, TextSlice, VersionVector, ID,
};
use std::{
    collections::BTreeMap,
    io,
    ops::Range,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
};

#[derive(Default)]
struct Store(RwLock<BTreeMap<u64, Vec<u8>>>, AtomicBool);
impl ExtentResolver for Store {
    fn with_bytes(
        &self,
        id: u64,
        range: Range<usize>,
        consumer: &'static str,
        visit: &mut dyn FnMut(&[u8]),
    ) -> io::Result<()> {
        if self.1.load(Ordering::SeqCst)
            && matches!(consumer, "arena.restore.validate" | "arena.adopt.index")
        {
            return Err(io::Error::new(io::ErrorKind::Other, "bound allocation must index the verified semantic text without a resolver callback"));
        }
        let guard = self.0.read().unwrap();
        visit(
            guard
                .get(&id)
                .and_then(|v| v.get(range))
                .ok_or(io::ErrorKind::NotFound)?,
        );
        Ok(())
    }
}
struct Allocator {
    store: Arc<Store>,
    context: Option<TextAllocationContext>,
    parts: Vec<(TextAllocationContext, TextSlice)>,
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
        let id = 100 + self.parts.len() as u64;
        let mut bytes = vec![0; len];
        generate(&mut bytes);
        self.store.0.write().unwrap().insert(id, bytes);
        let slice = TextSlice::extent(self.store.clone(), id, 0..len);
        self.parts
            .push((self.context.take().unwrap(), slice.clone()));
        Ok(slice)
    }
}
struct Ledger {
    store: Arc<Store>,
    op: ID,
    container: ContainerID,
    parts: Vec<TextSlice>,
    calls: Mutex<Vec<(ID, ContainerID, u32, usize)>>,
}
impl NativeWireLedger for Ledger {
    fn text_extents(
        &self,
        op: ID,
        container: &ContainerID,
        unicode_len: u32,
        byte_len: usize,
    ) -> LoroResult<Option<BoundTextExtents>> {
        self.calls
            .lock()
            .unwrap()
            .push((op, container.clone(), unicode_len, byte_len));
        if op != self.op || container != &self.container {
            return Ok(None);
        }
        Ok(Some(BoundTextExtents {
            provenance: ExtentProvenance::OriginalLocal,
            parts: self.parts.clone(),
        }))
    }
    fn verify_wire_receipt(&self, _: &[u8]) -> LoroResult<()> {
        panic!("typed import does not authenticate a wire receipt")
    }
    fn verify_typed_semantics(&self, _: &[u8]) -> LoroResult<()> {
        panic!("caller authenticates typed schema before binding")
    }
}
fn fixture() -> (LoroDoc, JsonSchema, Ledger) {
    let doc = LoroDoc::new();
    doc.set_peer_id(712).unwrap();
    doc.set_record_timestamp(false);
    doc.config_default_text_style(Some(StyleConfig {
        expand: ExpandType::Both,
    }));
    let meta = doc.get_map("meta");
    meta.insert("title", "document").unwrap();
    let list = meta.insert_container("items", LoroList::new()).unwrap();
    list.push("tag").unwrap();
    let text = meta.insert_container("body", LoroText::new()).unwrap();
    let mut allocator = Allocator {
        store: Arc::new(Store::default()),
        context: None,
        parts: vec![],
    };
    text.insert_unicode_with_allocator(0, "a玉", &mut allocator)
        .unwrap();
    text.insert_unicode_with_allocator(2, "🙂Z", &mut allocator)
        .unwrap();
    text.mark(1..3, "bold", true).unwrap();
    doc.set_next_commit_timestamp(1234);
    doc.set_next_commit_message("mixed metadata and text");
    doc.commit();
    let schema = doc
        .export_json_updates_without_peer_compression(&VersionVector::default(), &doc.oplog_vv());
    let ledger = Ledger {
        store: allocator.store.clone(),
        op: allocator.parts[0].0.id,
        container: allocator.parts[0].0.container.clone(),
        parts: allocator.parts.into_iter().map(|(_, s)| s).collect(),
        calls: Mutex::new(vec![]),
    };
    assert_eq!(
        schema
            .changes
            .iter()
            .flat_map(|c| &c.ops)
            .filter(|o| matches!(&o.content, JsonOpContent::Text(JsonTextOp::Insert { .. })))
            .count(),
        1
    );
    (doc, schema, ledger)
}
fn canonical(doc: &LoroDoc) -> serde_json::Value {
    serde_json::to_value(
        doc.export_json_updates_without_peer_compression(
            &VersionVector::default(),
            &doc.oplog_vv(),
        ),
    )
    .unwrap()
}
fn assert_unchanged(
    doc: &LoroDoc,
    before: &serde_json::Value,
    value: &loro::LoroValue,
    wire: &[u8],
) {
    assert_eq!(&canonical(doc), before);
    assert_eq!(&doc.get_deep_value(), value);
    assert_eq!(doc.export(ExportMode::all_updates()).unwrap(), wire);
}
#[test]
fn mixed_structure_style_import_retains_original_ordered_live_parts_and_exact_wire() {
    for compressed in [false, true] {
        let (source, schema, ledger) = fixture();
        let schema = if compressed {
            source.export_json_updates(&VersionVector::default(), &source.oplog_vv())
        } else {
            schema
        };
        let dst = LoroDoc::new();
        loro::reset_text_copy_metrics();
        let status = dst
            .import_json_updates_with_text_extents(schema, &ledger)
            .unwrap();
        assert!(status.pending.is_none());
        assert_eq!(canonical(&dst), canonical(&source));
        assert_eq!(dst.get_deep_value(), source.get_deep_value());
        assert_eq!(
            dst.get_text(ledger.container.clone()).get_richtext_value(),
            source
                .get_text(ledger.container.clone())
                .get_richtext_value()
        );
        assert_eq!(
            dst.export(ExportMode::all_updates()).unwrap(),
            source.export(ExportMode::all_updates()).unwrap()
        );
        assert_eq!(
            *ledger.calls.lock().unwrap(),
            vec![(ledger.op, ledger.container.clone(), 4, 9)]
        );
        let backing = dst.inspect_text_backings();
        let expected = vec![(Some(100), 0..4), (Some(101), 0..5)];
        for parts in [&backing.ops, &backing.arena] {
            assert_eq!(
                parts
                    .iter()
                    .map(|p| (p.extent, p.range.clone()))
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert!(!backing.state.is_empty());
        assert!(backing.state.iter().all(|p| p.kind == "extent"));
        let copies = loro::text_copy_metrics();
        assert!(
            copies
                .get("text_slice.extent_sequence_join")
                .is_some_and(|n| n.extent_bytes > 0),
            "temporary sequence reads are counted"
        );
        let branch = dst.fork_cow().unwrap();
        branch.set_peer_id(713).unwrap();
        branch.get_map("meta").insert("title", "branch").unwrap();
        branch
            .get_text(ledger.container.clone())
            .delete(1, 1)
            .unwrap();
        branch.commit();
        assert_eq!(canonical(&dst), canonical(&source));
        let root = dst.oplog_frontiers();
        branch.checkout(&root).unwrap();
        assert_eq!(branch.get_deep_value(), source.get_deep_value());
        branch.checkout_to_latest();
        drop(ledger);
        drop(source);
        drop(dst);
        assert!(branch
            .inspect_text_op_backings()
            .iter()
            .all(|p| p.kind == "extent"));
        let wire = branch.export(ExportMode::all_updates()).unwrap();
        let peer = LoroDoc::new();
        peer.import(&wire).unwrap();
        assert_eq!(peer.get_deep_value(), branch.get_deep_value());
    }
}
#[derive(Clone, Copy, Debug)]
enum Bad {
    Missing,
    Short,
    Duplicate,
    ErrorAfterVisit,
    Mismatch,
    Panic,
}
struct BadResolver {
    bad: Bad,
}
impl ExtentResolver for BadResolver {
    fn with_bytes(
        &self,
        _: u64,
        _: Range<usize>,
        _: &'static str,
        visit: &mut dyn FnMut(&[u8]),
    ) -> io::Result<()> {
        match self.bad {
            Bad::Missing => Ok(()),
            Bad::Short => {
                visit(b"x");
                Ok(())
            }
            Bad::Duplicate => {
                visit("a玉🙂Z".as_bytes());
                visit("a玉🙂Z".as_bytes());
                Ok(())
            }
            Bad::ErrorAfterVisit => {
                visit("a玉🙂Z".as_bytes());
                Err(io::ErrorKind::NotFound.into())
            }
            Bad::Mismatch => {
                visit("b玉🙂Z".as_bytes());
                Ok(())
            }
            Bad::Panic => panic!("resolver failed"),
        }
    }
}
#[test]
fn invalid_bindings_callbacks_and_later_schema_leave_state_history_unchanged() {
    let (_, schema, mut ledger) = fixture();
    let dst = LoroDoc::new();
    dst.set_peer_id(900).unwrap();
    dst.get_map("keep").insert("ok", true).unwrap();
    dst.commit();
    let before = canonical(&dst);
    let value = dst.get_deep_value();
    let wire = dst.export(ExportMode::all_updates()).unwrap();
    let valid = ledger.parts.clone();
    for bad in [
        Bad::Missing,
        Bad::Short,
        Bad::Duplicate,
        Bad::ErrorAfterVisit,
        Bad::Mismatch,
        Bad::Panic,
    ] {
        ledger.parts = vec![TextSlice::extent(Arc::new(BadResolver { bad }), 500, 0..9)];
        assert!(
            dst.import_json_updates_with_text_extents(schema.clone(), &ledger)
                .is_err(),
            "{bad:?}"
        );
        assert_unchanged(&dst, &before, &value, &wire);
    }
    for parts in [
        vec![],
        vec![TextSlice::from_bytes("a玉🙂Z".as_bytes())],
        vec![valid[0].clone()],
        vec![valid[0].clone(), valid[0].clone()],
    ] {
        ledger.parts = parts;
        assert!(dst
            .import_json_updates_with_text_extents(schema.clone(), &ledger)
            .is_err());
        assert_unchanged(&dst, &before, &value, &wire);
    }
    ledger.parts = valid;
    let original_op = ledger.op;
    ledger.op.counter += 1;
    assert!(dst
        .import_json_updates_with_text_extents(schema.clone(), &ledger)
        .is_err());
    assert_unchanged(&dst, &before, &value, &wire);
    ledger.op = original_op;
    let mut invalid = schema.clone();
    invalid.changes[0].ops.last_mut().unwrap().counter += 1;
    assert!(dst
        .import_json_updates_with_text_extents(invalid, &ledger)
        .is_err());
    assert_unchanged(&dst, &before, &value, &wire);
    let mut invalid = schema.clone();
    invalid.schema_version = 255;
    assert!(dst
        .import_json_updates_with_text_extents(invalid, &ledger)
        .is_err());
    assert_unchanged(&dst, &before, &value, &wire);
    dst.import_json_updates_with_text_extents(schema, &ledger)
        .unwrap();
    assert_eq!(
        dst.inspect_text_op_backings()
            .iter()
            .map(|p| p.extent)
            .collect::<Vec<_>>(),
        vec![Some(100), Some(101)]
    );
}
#[test]
fn bound_pending_dependency_keeps_extents_until_parent_arrives() {
    let (source, schema, ledger) = fixture();
    // Split an actual native change at the insertion start, retaining counter/dependency grammar.
    let mut prefix_vv = VersionVector::default();
    prefix_vv.set_end(ledger.op);
    let prefix =
        source.export_json_updates_without_peer_compression(&VersionVector::default(), &prefix_vv);
    let suffix =
        source.export_json_updates_without_peer_compression(&prefix_vv, &source.oplog_vv());
    let dst = LoroDoc::new();
    assert!(dst
        .import_json_updates_with_text_extents(suffix, &ledger)
        .unwrap()
        .pending
        .is_some());
    assert!(dst.oplog_vv().is_empty());
    assert!(dst
        .import_json_updates_with_text_extents(prefix, &ledger)
        .unwrap()
        .pending
        .is_none());
    assert_eq!(canonical(&dst), serde_json::to_value(schema).unwrap());
    assert_eq!(
        dst.inspect_text_op_backings()
            .iter()
            .map(|p| p.extent)
            .collect::<Vec<_>>(),
        vec![Some(100), Some(101)]
    );
}
#[test]
fn provider_does_not_turn_ledger_failure_into_owned_fallback() {
    struct Reject;
    impl NativeWireLedger for Reject {
        fn text_extents(
            &self,
            _: ID,
            _: &ContainerID,
            _: u32,
            _: usize,
        ) -> LoroResult<Option<BoundTextExtents>> {
            Err(LoroError::DecodeError("ledger rejected".into()))
        }
        fn verify_wire_receipt(&self, _: &[u8]) -> LoroResult<()> {
            unreachable!()
        }
        fn verify_typed_semantics(&self, _: &[u8]) -> LoroResult<()> {
            unreachable!()
        }
    }
    let (_, schema, _) = fixture();
    let dst = LoroDoc::new();
    assert!(dst
        .import_json_updates_with_text_extents(schema, &Reject)
        .is_err());
    assert!(dst.oplog_vv().is_empty());
}

#[test]
fn precomposed_ledger_operand_preserves_original_parts() {
    let (source, schema, mut ledger) = fixture();
    let mut composite = ledger.parts[0].clone();
    composite.try_merge(&ledger.parts[1]).unwrap();
    ledger.parts = vec![composite];
    let dst = LoroDoc::new();
    dst.import_json_updates_with_text_extents(schema, &ledger)
        .unwrap();
    assert_eq!(canonical(&dst), canonical(&source));
    assert_eq!(
        dst.inspect_text_op_backings()
            .iter()
            .map(|p| (p.extent, p.range.clone()))
            .collect::<Vec<_>>(),
        vec![(Some(100), 0..4), (Some(101), 0..5)]
    );
}

#[test]
fn bound_import_indexes_verified_text_without_arena_resolver_callbacks() {
    let (source, schema, ledger) = fixture();
    ledger.store.1.store(true, Ordering::SeqCst);
    let dst = LoroDoc::new();
    dst.import_json_updates_with_text_extents(schema, &ledger)
        .unwrap();
    assert_eq!(canonical(&dst), canonical(&source));
    assert_eq!(dst.get_deep_value(), source.get_deep_value());
    assert!(dst
        .inspect_text_backings()
        .state
        .iter()
        .all(|part| part.kind == "extent"));
}
