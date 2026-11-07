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

#[test]
fn reference_only_mixed_import_checks_lengths_and_retains_original_parts() {
    use loro::native_wire_recipe::{NativeExtentRef, NativeTextReference};
    let (source, mut schema, ledger) = fixture();
    let mut references = Vec::new();
    for change in &mut schema.changes {
        for op in &mut change.ops {
            if let JsonOpContent::Text(JsonTextOp::Insert { text, .. }) = &mut op.content {
                references.push(NativeTextReference {
                    op: ID::new(change.id.peer, op.counter), container: op.container.clone(),
                    unicode_len: text.chars().count() as u32, byte_len: text.len(),
                    provenance: ExtentProvenance::OriginalLocal,
                    extents: ledger.parts.iter().flat_map(TextSlice::backing_parts).map(|p| NativeExtentRef {
                        extent: p.extent.unwrap(), start: p.range.start, end: p.range.end,
                    }).collect(),
                });
                text.clear();
            }
        }
    }
    for fault in 0..5 {
        let dst = LoroDoc::new();
        let mut bad = references.clone();
        match fault {
            0 => bad[0].unicode_len += 1,
            1 => bad[0].byte_len += 1,
            2 => bad[0].extents[0].extent += 123,
            3 => bad.push(bad[0].clone()),
            4 => bad.clear(),
            _ => unreachable!(),
        }
        assert!(dst.import_json_updates_with_text_references(schema.clone(), &bad, &ledger).is_err());
        assert!(dst.oplog_vv().is_empty());
    }
    let dst = LoroDoc::new();
    dst.import_json_updates_with_text_references(schema, &references, &ledger).unwrap();
    assert_eq!(canonical(&dst), canonical(&source));
    assert_eq!(dst.get_deep_value(), source.get_deep_value());
    assert!(dst.inspect_text_op_backings().iter().all(|p| p.extent.is_some()));
}

#[test]
fn text_slice_export_keeps_original_operands_and_nontext_grammar() {
    let (source, _, _) = fixture();
    let expected = canonical(&source);
    let (mut schema, slices) = source.export_json_updates_with_text_slices(
        &VersionVector::default(), &source.oplog_vv());
    assert!(!slices.is_empty());
    for change in &mut schema.changes {
        for op in &mut change.ops {
            if let JsonOpContent::Text(JsonTextOp::Insert { text, .. }) = &mut op.content {
                assert!(text.is_empty());
                let slot = slices.iter().find(|s| s.op == ID::new(change.id.peer, op.counter)).unwrap();
                assert_eq!(slot.container, op.container);
                assert!(slot.slice.is_extent());
                slot.slice.try_for_each_char("test.export", |c| { text.push(c); Ok(()) }).unwrap();
                assert_eq!(text.chars().count(), slot.unicode_len as usize);
            }
        }
    }
    assert_eq!(serde_json::to_value(schema).unwrap(), expected);
    for slot in slices {
        let (_, pieces) = source.export_json_in_id_span_with_text_slices(
            loro::IdSpan::new(slot.op.peer, slot.op.counter, slot.op.counter + slot.unicode_len as i32));
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0].slice.try_bytes_equal(&slot.slice).unwrap());
        if slot.unicode_len > 2 {
            loro::reset_text_copy_metrics();
            let (_, middle) = source.export_json_in_id_span_with_text_slices(loro::IdSpan::new(
                slot.op.peer, slot.op.counter + 1, slot.op.counter + slot.unicode_len as i32 - 1));
            let counts = loro::text_copy_metrics();
            assert!(!counts.contains_key("text_slice.extent_sequence_join"));
            assert!(!counts.contains_key("oracle.json_export"));
            assert_eq!(middle.len(), 1);
            assert_eq!(middle[0].unicode_len, slot.unicode_len - 2);
            assert!(middle[0].slice.is_extent());
        }
    }
}

impl loro::native_reference_oracle::NativeTextLookup for Ledger {
    fn text_extents(&self, op: ID, container: &ContainerID, unicode_len: u32, byte_len: usize) -> LoroResult<Option<BoundTextExtents>> {
        NativeWireLedger::text_extents(self, op, container, unicode_len, byte_len)
    }
}

#[test]
fn native_reference_update_oracle_is_wire_driven_and_keeps_extent_history() {
    use loro::native_reference_oracle::NativeReferenceOracle;
    let (source, _, ledger) = fixture();
    let wire = source.export(ExportMode::all_updates()).unwrap();
    let base = LoroDoc::new();
    let base_before = canonical(&base);
    loro::reset_text_copy_metrics();
    let oracle = NativeReferenceOracle::from_update(&base, &wire, Arc::new(ledger)).unwrap();
    assert!(!loro::text_copy_metrics().contains_key("native.decode.text_arena"));
    assert_eq!(oracle.oplog_vv(), source.oplog_vv());
    assert_eq!(oracle.state_vv(), source.state_vv());
    assert_eq!(oracle.get_deep_value(), source.get_deep_value());
    let (_, slices) = oracle.history(&VersionVector::default(), &oracle.oplog_vv());
    assert!(!slices.is_empty());
    assert!(slices.iter().all(|s| s.slice.is_extent()));
    assert_eq!(canonical(&base), base_before);
    let (_, again) = oracle.history(&VersionVector::default(), &oracle.oplog_vv());
    assert_eq!(slices[0].slice.backing_parts(), again[0].slice.backing_parts());
}

#[test]
fn native_reference_update_rejects_missing_mismatched_and_panicking_lookup() {
    use loro::native_reference_oracle::{NativeReferenceOracle, NativeTextLookup};
    struct Bad(u8);
    impl NativeTextLookup for Bad {
        fn text_extents(&self, _: ID, _: &ContainerID, _: u32, _: usize) -> LoroResult<Option<BoundTextExtents>> {
            if self.0 == 1 { panic!("lookup failure"); }
            Ok(None)
        }
    }
    let (source, _, mut ledger) = fixture();
    let wire = source.export(ExportMode::all_updates()).unwrap();
    let base = LoroDoc::new();
    for fault in [0, 1] {
        assert!(NativeReferenceOracle::from_update(&base, &wire, Arc::new(Bad(fault))).is_err());
        assert!(base.oplog_vv().is_empty());
    }
    let first = ledger.parts[0].backing_info();
    ledger.store.0.write().unwrap().get_mut(&first.extent.unwrap()).unwrap()[first.range.start] = b'!';
    assert!(NativeReferenceOracle::from_update(&base, &wire, Arc::new(ledger)).is_err());
    assert!(base.oplog_vv().is_empty());
}

#[test]
fn native_reference_update_rejects_pending_base_without_committing_it() {
    use loro::native_reference_oracle::NativeReferenceOracle;
    let (source, _, ledger) = fixture();
    let wire = source.export(ExportMode::all_updates()).unwrap();
    let base = LoroDoc::new();
    base.get_text("pending").insert(0, "not committed").unwrap();
    let pending = base.get_pending_txn_len();
    assert!(pending > 0);
    assert!(NativeReferenceOracle::from_update(&base, &wire, Arc::new(ledger)).is_err());
    assert_eq!(base.get_pending_txn_len(), pending);
}

#[test]
fn native_reference_update_returns_error_when_resolver_panics_after_binding() {
    use loro::native_reference_oracle::{NativeReferenceOracle, NativeTextLookup};
    use std::sync::atomic::AtomicUsize;
    struct Late { reads: AtomicUsize }
    impl ExtentResolver for Late {
        fn with_bytes(&self, _: u64, range: Range<usize>, _: &'static str, visit: &mut dyn FnMut(&[u8])) -> io::Result<()> {
            if self.reads.fetch_add(1, Ordering::SeqCst) != 0 { panic!("late resolver failure"); }
            visit(&"a玉🙂Z".as_bytes()[range]); Ok(())
        }
    }
    struct Lookup(Arc<Late>);
    impl NativeTextLookup for Lookup {
        fn text_extents(&self, _: ID, _: &ContainerID, _: u32, _: usize) -> LoroResult<Option<BoundTextExtents>> {
            Ok(Some(BoundTextExtents { provenance: ExtentProvenance::OriginalLocal,
                parts: vec![TextSlice::extent(self.0.clone(), 1, 0..9)] }))
        }
    }
    let (source, _, _) = fixture();
    let wire = source.export(ExportMode::all_updates()).unwrap();
    let base = LoroDoc::new();
    let lookup = Arc::new(Lookup(Arc::new(Late { reads: AtomicUsize::new(0) })));
    let released = Arc::downgrade(&lookup.0);
    assert!(NativeReferenceOracle::from_update(&base, &wire, lookup).is_err());
    assert!(base.oplog_vv().is_empty());
    assert!(released.upgrade().is_none(), "failed private oracle must release its resolver owner");
}

#[test]
fn native_reference_snapshot_accepts_empty_auto_commit_configuration() {
    use loro::native_reference_oracle::NativeReferenceOracle;
    let source = LoroDoc::new();
    source.set_peer_id(913).unwrap();
    let mut allocator = Allocator { store: Arc::new(Store::default()), context: None, parts: vec![] };
    source.get_text("body").insert_unicode_with_allocator(0, "a玉🙂Z", &mut allocator).unwrap();
    source.commit();
    let ledger = Arc::new(Ledger {
        store: allocator.store.clone(), op: allocator.parts[0].0.id,
        container: allocator.parts[0].0.container.clone(),
        parts: allocator.parts.into_iter().map(|(_, part)| part).collect(),
        calls: Mutex::new(vec![]),
    });
    let wire = source.export(ExportMode::Snapshot).unwrap();
    // Public new() opens an empty auto-commit transaction; fork_cow inherits it.
    let config = LoroDoc::new();
    config.config_default_text_style(Some(StyleConfig { expand: ExpandType::Both }));
    assert_eq!(config.get_pending_txn_len(), 0);
    let before = config.oplog_vv();
    let oracle = NativeReferenceOracle::from_snapshot_with_config(&config, &wire, ledger.clone()).unwrap();
    assert_eq!(oracle.oplog_vv(), source.oplog_vv());
    assert!(oracle.get_deep_value_with_text_slices().unwrap()
        .same_value(&source.get_deep_value_with_text_slices().unwrap()).unwrap());
    assert_eq!(config.oplog_vv(), before);
    assert_eq!(config.get_pending_txn_len(), 0);
    // Reject a genuinely pending caller before any implicit commit/fork.
    config.get_text("uncommitted").insert(0, "preserve caller draft").unwrap();
    let pending = config.get_pending_txn_len();
    assert!(pending > 0);
    assert!(NativeReferenceOracle::from_snapshot_with_config(&config, &wire, ledger).is_err());
    assert_eq!(config.get_pending_txn_len(), pending);
}

// Independent counter-positive controls and public native-oracle lifetime checks.
#[test]
fn independent_native_decoder_update_has_positive_legacy_copy_control() {
    use loro::native_reference_oracle::NativeReferenceOracle;
    let (source, _, ledger) = fixture();
    let wire = source.export(ExportMode::all_updates()).unwrap();
    let expected = source.get_deep_value();
    let ordinary = LoroDoc::new();
    loro::reset_text_copy_metrics();
    ordinary.import(&wire).unwrap();
    let legacy = loro::text_copy_metrics();
    assert_eq!(legacy["native.decode.text_arena"].native_bytes, 9);
    let base = LoroDoc::new();
    loro::reset_text_copy_metrics();
    let oracle = NativeReferenceOracle::from_update(&base, &wire, Arc::new(ledger)).unwrap();
    let reference = loro::text_copy_metrics();
    assert!(!reference.contains_key("native.decode.text_arena"));
    assert!(!reference.contains_key("text_slice.extent_sequence_join"));
    eprintln!("INDEPENDENT_NATIVE_UPDATE_COUNTERS legacy={} reference={}",
        serde_json::to_string(&legacy).unwrap(), serde_json::to_string(&reference).unwrap());
    drop(wire); drop(source);
    let (_, history) = oracle.history(&VersionVector::default(), &oracle.oplog_vv());
    assert!(!history.is_empty());
    assert!(history.iter().all(|x| x.slice.is_extent()));
    assert_eq!(oracle.get_deep_value(), expected);
    assert!(base.oplog_vv().is_empty());
}

#[test]
fn independent_native_decoder_snapshot_retains_originals_after_wire_drop() {
    use loro::native_reference_oracle::NativeReferenceOracle;
    let source = LoroDoc::new(); source.set_peer_id(712).unwrap();
    source.set_record_timestamp(false);
    let text = source.get_text("body");
    let mut allocator = Allocator { store: Arc::new(Store::default()), context: None, parts: vec![] };
    text.insert_unicode_with_allocator(0, "a玉", &mut allocator).unwrap();
    text.insert_unicode_with_allocator(2, "🙂Z", &mut allocator).unwrap(); source.commit();
    let ledger = Ledger { store: allocator.store.clone(), op: allocator.parts[0].0.id,
        container: allocator.parts[0].0.container.clone(),
        parts: allocator.parts.into_iter().map(|(_, slice)| slice).collect(), calls: Mutex::new(vec![]) };
    let wire = source.export(ExportMode::Snapshot).unwrap();
    let expected = source.get_deep_value();
    let base = LoroDoc::new();
    loro::reset_text_copy_metrics();
    let oracle = NativeReferenceOracle::from_snapshot_with_config(&base, &wire, Arc::new(ledger)).unwrap();
    let projection = oracle.get_deep_value_with_text_slices().unwrap();
    let counts = loro::text_copy_metrics();
    assert!(!counts.contains_key("native.snapshot.decode_string"));
    assert!(!counts.contains_key("native.snapshot.text_arena"));
    assert!(!counts.contains_key("native.decode.text_arena"));
    assert!(!counts.contains_key("text_slice.extent_sequence_join"));
    assert!(counts.get("oracle.snapshot.encoded_body").is_some_and(|c| c.native_bytes > 0),
        "the required encoded input copy must remain visible");
    assert_eq!(projection.text.len(), 1);
    assert!(!projection.text[0].runs.is_empty());
    assert!(projection.text[0].runs.iter().all(|run| run.slice.is_extent()));
    eprintln!("INDEPENDENT_NATIVE_SNAPSHOT_COUNTERS {}", serde_json::to_string(&counts).unwrap());
    drop(wire); drop(source); drop(text);
    let (_, history) = oracle.history(&VersionVector::default(), &oracle.oplog_vv());
    assert!(history.iter().all(|x| x.slice.is_extent()));
    assert_eq!(oracle.get_deep_value(), expected);
    assert!(base.oplog_vv().is_empty());
}

#[test]
fn independent_native_decoder_ordinary_drop_still_commits_pending_edits() {
    let events=Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let subscription;
    {
        let doc=LoroDoc::new(); doc.set_peer_id(991).unwrap();
        let output=events.clone();
        subscription=doc.subscribe_local_update(Box::new(move |bytes| {
            output.lock().unwrap().push(bytes.to_vec()); true
        }));
        doc.get_text("body").insert(0,"ordinary Drop玉🙂").unwrap();
        assert!(doc.get_pending_txn_len()>0);
        assert!(events.lock().unwrap().is_empty(),"fixture committed before Drop");
    }
    let updates=events.lock().unwrap();
    assert!(!updates.is_empty(),"ordinary final-owner Drop lost its implicit commit");
    let restored=LoroDoc::new();
    for update in updates.iter(){restored.import(update).unwrap();}
    assert_eq!(restored.get_text("body").to_string(),"ordinary Drop玉🙂");
    drop(subscription);
}

#[test]
fn independent_native_decoder_snapshot_failure_sweep_keeps_caller_and_releases_owners() {
    use loro::native_reference_oracle::{NativeReferenceOracle, NativeTextLookup};
    use std::sync::atomic::AtomicUsize;
    struct Faults { calls: Arc<AtomicUsize>, fail_at:usize, panic:bool }
    impl ExtentResolver for Faults {
        fn with_bytes(&self, _:u64, range:Range<usize>, _: &'static str, visit:&mut dyn FnMut(&[u8])) -> io::Result<()> {
            if self.calls.fetch_add(1,Ordering::SeqCst)==self.fail_at {
                if self.panic { panic!("independent snapshot late resolver failure"); }
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            visit(&b"abc"[range]); Ok(())
        }
    }
    struct Lookup(Arc<Faults>);
    impl NativeTextLookup for Lookup {
        fn text_extents(&self, _:ID, _: &ContainerID, _:u32, _:usize) -> LoroResult<Option<BoundTextExtents>> {
            Ok(Some(BoundTextExtents { provenance:ExtentProvenance::OriginalLocal,
                parts:vec![TextSlice::extent(self.0.clone(),1,0..3)] }))
        }
    }
    let source=LoroDoc::new();source.set_peer_id(80).unwrap();
    source.get_text("body").insert(0,"abc").unwrap();source.commit();
    let wire=source.export(ExportMode::Snapshot).unwrap();let mut exercised_late=false;
    for panic in [false,true] { for fail_at in 0..12 {
        let base=LoroDoc::new();let calls=Arc::new(AtomicUsize::new(0));
        let resolver=Arc::new(Faults {calls:calls.clone(),fail_at,panic});
        let released=Arc::downgrade(&resolver);
        let result=NativeReferenceOracle::from_snapshot_with_config(&base,&wire,Arc::new(Lookup(resolver)));
        let reached=calls.load(Ordering::SeqCst)>fail_at;
        assert_eq!(result.is_err(),reached,"unexpected result at read {fail_at}, panic={panic}");
        exercised_late|=reached && fail_at>0;
        assert_eq!(base.get_pending_txn_len(),0);
        assert!(base.oplog_vv().is_empty());assert!(base.state_vv().is_empty());
        drop(result);
        assert!(released.upgrade().is_none(),"failed/finished candidate leaked its resolver");
        eprintln!("INDEPENDENT_SNAPSHOT_FAILURE_SWEEP panic={panic} fail_at={fail_at} reads={} reached={reached}",calls.load(Ordering::SeqCst));
    }}
    assert!(exercised_late,"fault sweep never reached a post-binding callback");
}
