#![cfg(feature = "shared_arena_experiment")]
//! Trusted callback admission and an explicit negative control for capability
//! isolation. These callbacks are fixed application code, not untrusted plugins.
use loro::{
    native_reference_oracle::{NativeReferenceOracle, NativeTextLookup},
    native_wire_recipe::{BoundTextExtents, ExtentProvenance},
    ContainerID, ExportMode, ExtentResolver, Frontiers, ImportStatus, LoroDoc,
    LoroError, LoroResult, TextAllocationContext, TextExtentAllocator, TextSlice, ID,
};
use std::{io, ops::Range, sync::{Arc, Mutex, Weak}};

#[derive(Default)]
struct Original(Mutex<Vec<u8>>);
impl ExtentResolver for Original {
    fn with_bytes(&self, _: u64, range: Range<usize>, _: &'static str,
        visit: &mut dyn FnMut(&[u8])) -> io::Result<()> {
        let bytes = self.0.lock().unwrap();
        visit(bytes.get(range).ok_or(io::ErrorKind::InvalidData)?);
        Ok(())
    }
}
struct Allocation { owner: Arc<Original>, context: Option<TextAllocationContext> }
impl TextExtentAllocator for Allocation {
    fn begin_allocation(&mut self, context: &TextAllocationContext) -> io::Result<()> {
        self.context = Some(context.clone()); Ok(())
    }
    fn allocate_text(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> io::Result<TextSlice> {
        let mut bytes = vec![0; len]; generate(&mut bytes);
        *self.owner.0.lock().unwrap() = bytes;
        Ok(TextSlice::extent(self.owner.clone(), 1, 0..len))
    }
}
struct Lookup { owner: Arc<Original>, id: ID, container: ContainerID }
impl NativeTextLookup for Lookup {
    fn text_extents(&self, id: ID, container: &ContainerID, _: u32, bytes: usize) -> LoroResult<Option<BoundTextExtents>> {
        if id != self.id || container != &self.container { return Ok(None); }
        Ok(Some(BoundTextExtents { provenance: ExtentProvenance::OriginalLocal,
            parts: vec![TextSlice::extent(self.owner.clone(), 1, 0..bytes)] }))
    }
}
fn fixture() -> (Vec<u8>, Vec<u8>, Arc<Lookup>, Weak<Original>, ImportStatus) {
    let source = LoroDoc::new(); source.set_peer_id(1903).unwrap();
    let mut allocation = Allocation { owner: Arc::new(Original::default()), context: None };
    source.get_text("body").insert_unicode_with_allocator(0, "a玉🙂", &mut allocation).unwrap();
    source.get_map("meta").insert("key", "value").unwrap(); source.commit();
    let wire = source.export(ExportMode::all_updates()).unwrap();
    let snapshot = source.export(ExportMode::Snapshot).unwrap();
    let context = allocation.context.take().unwrap();
    let released = Arc::downgrade(&allocation.owner);
    let lookup = Arc::new(Lookup { owner: allocation.owner, id: context.id, container: context.container });
    let independent = LoroDoc::new();
    let actual_status = independent.import(&wire).unwrap();
    (wire, snapshot, lookup, released, actual_status)
}

#[test]
fn trusted_validation_read_receives_real_import_status() {
    let (wire, _, lookup, released, expected_status) = fixture();
    let base = LoroDoc::new(); base.set_peer_id(1904).unwrap();
    base.get_map("base_only").insert("prior", 1).unwrap(); base.commit();
    let base_before = base.oplog_vv();
    let base_peer = base.peer_id();
    let oracle = NativeReferenceOracle::from_update(&base, &wire, lookup).unwrap();
    let (oracle, value) = oracle.trusted_validation::<_, LoroError>(|doc, status| {
        assert_eq!(status, &expected_status);
        assert!(status.success.get(&base_peer).is_none(), "status must not be fabricated from final VV");
        assert!(doc.oplog_vv().get(&base_peer).is_some());
        assert_eq!(doc.get_text("body").to_string(), "a玉🙂");
        Ok(37)
    }).unwrap();
    assert_eq!(value, 37);
    assert_eq!(oracle.get_pending_txn_len(), 0);
    assert_eq!(base.oplog_vv(), base_before);
    drop(oracle); assert!(released.upgrade().is_none());
}

#[test]
fn trusted_validation_container_identity_does_not_enable_auto_commit() {
    use loro::ContainerTrait;

    let (wire, _, lookup, released, _) = fixture();
    let base = LoroDoc::new();
    let oracle = NativeReferenceOracle::from_update(&base, &wire, lookup).unwrap();
    let (oracle, ()) = oracle.trusted_validation::<_, LoroError>(|doc, _| {
        let text = doc.get_text("body");
        assert!(text.is_attached_to(doc));
        assert!(!text.is_attached_to(&base));
        assert!(!loro::LoroText::new().is_attached_to(doc));
        assert_eq!(doc.get_pending_txn_len(), 0);
        Ok(())
    }).unwrap();
    assert_eq!(oracle.get_pending_txn_len(), 0);
    drop(oracle);
    assert!(released.upgrade().is_none());
}

#[test]
fn trusted_validation_rejects_mutation_error_and_panic_without_publishing_results() {
    for fault in 0..5 {
        let (wire, _, lookup, released, _) = fixture();
        let base = LoroDoc::new();
        let oracle = NativeReferenceOracle::from_update(&base, &wire, lookup).unwrap();
        let result = oracle.trusted_validation::<_, LoroError>(|doc, _| {
            match fault {
                0 | 1 => {
                    doc.inner().start_auto_commit();
                    doc.get_text("body").insert(0, "changed").unwrap();
                    if fault == 1 { doc.commit(); }
                }
                2 => doc.checkout(&Frontiers::default())?,
                3 => return Err(LoroError::DecodeError("application rejected".into())),
                4 => {
                    doc.inner().start_auto_commit();
                    doc.get_map("private_change").insert("value", 1).unwrap();
                    panic!("application validation panicked");
                }
                _ => unreachable!(),
            }
            Ok("must not escape")
        });
        assert!(result.is_err(), "fault {fault} returned an oracle/proof");
        assert!(base.oplog_vv().is_empty()); assert_eq!(base.get_pending_txn_len(), 0);
        assert!(released.upgrade().is_none(), "fault {fault} leaked the private candidate");
        base.get_map("still_usable").insert("value", 1).unwrap(); base.commit();
    }
}

#[test]
fn trusted_validation_snapshot_has_no_fabricated_update_status() {
    let (_, snapshot, lookup, released, _) = fixture();
    let oracle = NativeReferenceOracle::from_snapshot(&snapshot, lookup).unwrap();
    let invoked = std::cell::Cell::new(false);
    let result = oracle.trusted_validation::<_, LoroError>(|_, _| { invoked.set(true); Ok(()) });
    assert!(result.is_err()); assert!(!invoked.get()); assert!(released.upgrade().is_none());
}

#[test]
fn negative_control_trusted_validation_fence_is_not_hard_capability_isolation() {
    let (wire, _, lookup, released, _) = fixture();
    let base = LoroDoc::new();
    let oracle = NativeReferenceOracle::from_update(&base, &wire, lookup).unwrap();
    let mut escaped = None;
    let (oracle, changed_recording) = oracle.trusted_validation::<_, LoroError>(|doc, _| {
        // Deliberately violate the trusted callback contract. Neither cloning a
        // handle nor changing this configuration moves VV/frontiers/pending.
        let next = !doc.config().record_timestamp();
        doc.set_record_timestamp(next);
        escaped = Some(doc.clone());
        Ok(next)
    }).unwrap();
    assert_eq!(escaped.as_ref().unwrap().config().record_timestamp(), changed_recording);
    // A saved clone can mutate *after* a successful fence. This is the explicit
    // negative result: trusted_validation is not a hard read-only capability.
    escaped.as_ref().unwrap().inner().start_auto_commit();
    escaped.as_ref().unwrap().get_map("escaped_mutation").insert("value", 1).unwrap();
    assert!(oracle.get_pending_txn_len() > 0, "must report real pending state, not a constant zero");
    drop(oracle);
    assert!(released.upgrade().is_some(), "escaped handle really retains the owner");
    drop(escaped); assert!(released.upgrade().is_none());
    assert!(base.oplog_vv().is_empty());
}

// Independent resume control: metadata describes all incoming operations, even
// when native import later ignores or trims already-known operations.
#[test]
fn independent_reference_update_metadata_matches_wire_before_known_history_filtering() {
    fn same(a: &loro::ImportBlobMetadata, b: &loro::ImportBlobMetadata) {
        assert_eq!(a.partial_start_vv, b.partial_start_vv);
        assert_eq!(a.partial_end_vv, b.partial_end_vv);
        assert_eq!(a.start_timestamp, b.start_timestamp);
        assert_eq!(a.start_frontiers, b.start_frontiers);
        assert_eq!(a.end_timestamp, b.end_timestamp);
        assert_eq!(a.change_num, b.change_num);
        assert_eq!(a.mode, b.mode);
    }
    struct MetadataLedger<'a>(&'a Lookup);
    impl loro::native_wire_recipe::NativeWireLedger for MetadataLedger<'_> {
        fn text_extents(&self, id: ID, container: &ContainerID, unicode: u32, bytes: usize) -> LoroResult<Option<BoundTextExtents>> {
            NativeTextLookup::text_extents(self.0, id, container, unicode, bytes)
        }
        fn verify_wire_receipt(&self, _: &[u8]) -> LoroResult<()> { panic!("typed prefix import has no wire receipt") }
        fn verify_typed_semantics(&self, _: &[u8]) -> LoroResult<()> { panic!("typed prefix import has no wire verifier") }
    }
    let (wire, snapshot, lookup, _, _) = fixture();
    loro::reset_text_copy_metrics();
    let expected = LoroDoc::decode_import_blob_meta(&wire, true).unwrap();
    let control = loro::text_copy_metrics();
    assert!(control.get("native.decode.text_arena").is_some_and(|c| c.native_bytes > 0));
    let source = LoroDoc::new(); source.import(&wire).unwrap();
    for known in [0, 1, *source.oplog_vv().get(&1903).unwrap()] {
        let base = LoroDoc::new(); base.set_peer_id(1904).unwrap();
        base.get_map("unrelated_base_peer").insert("prior", 1).unwrap(); base.commit();
        if known > 0 {
            let mut prefix = loro::VersionVector::default(); prefix.insert(1903, known);
            let prefix_doc = LoroDoc::new();
            prefix_doc.import(&source.export(ExportMode::updates_till(&prefix)).unwrap()).unwrap();
            let schema = prefix_doc.export_json_updates_without_peer_compression(&loro::VersionVector::default(), &prefix_doc.oplog_vv());
            base.import_json_updates_with_text_extents(schema, &MetadataLedger(lookup.as_ref())).unwrap();
        }
        let before = base.oplog_vv();
        loro::reset_text_copy_metrics();
        let oracle = NativeReferenceOracle::from_update(&base, &wire, lookup.clone()).unwrap();
        let metadata = oracle.update_metadata().expect("real Update metadata");
        same(metadata, &expected);
        assert!(metadata.partial_end_vv.get(&1904).is_none());
        assert!(oracle.oplog_vv().get(&1904).is_some());
        let copies = loro::text_copy_metrics();
        assert_eq!(copies.get("native.decode.text_arena").map_or(0, |c| c.native_bytes+c.extent_bytes), 0);
        assert_eq!(base.oplog_vv(), before);
        eprintln!("RESUME_METADATA known={known} metadata={metadata:?} copies={copies:?}");
    }
    let snapshot_oracle = NativeReferenceOracle::from_snapshot(&snapshot, lookup.clone()).unwrap();
    assert!(snapshot_oracle.update_metadata().is_none());
    let mut corrupt = wire.clone(); *corrupt.last_mut().unwrap() ^= 0xff;
    for rejected in [corrupt, snapshot, vec![], vec![0]] {
        let base = LoroDoc::new();
        assert!(NativeReferenceOracle::from_update(&base, &rejected, lookup.clone()).is_err());
        assert!(base.oplog_vv().is_empty());
        assert_eq!(base.get_pending_txn_len(), 0);
    }
}
