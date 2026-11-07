#![cfg(feature = "shared_arena_experiment")]
//! Preimplementation contracts. The ledger originates at real allocation hooks;
//! byte equality alone is never authority to bind an extent to a native operand.
use loro::native_wire_recipe::{
    capture_native_wire_recipe, reconstruct_native_wire_recipe, BoundTextExtents, ExtentProvenance,
    NativeWireLedger, NativeWireRecipe,
};
use loro::{
    ContainerID, ExportMode, ExtentResolver, JsonOpContent, JsonTextOp, LoroDoc, LoroError,
    LoroResult, TextAllocationContext, TextExtentAllocator, TextSlice, VersionVector, ID,
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
#[derive(Clone)]
struct Binding {
    id: ID,
    container: ContainerID,
    parts: Vec<TextSlice>,
}
#[derive(Clone)]
struct Ledger {
    wire: Vec<u8>,
    typed: serde_json::Value,
    bindings: Vec<Binding>,
    provenance: ExtentProvenance,
}
fn invalid(reason: &str) -> LoroError {
    LoroError::DecodeError(reason.into())
}
impl NativeWireLedger for Ledger {
    fn text_extents(
        &self,
        op: ID,
        container: &ContainerID,
        unicode_len: u32,
        byte_len: usize,
    ) -> LoroResult<Option<BoundTextExtents>> {
        let end = op
            .counter
            .checked_add(i32::try_from(unicode_len).map_err(|_| invalid("counter overflow"))?)
            .ok_or_else(|| invalid("counter overflow"))?;
        let mut selected = vec![];
        for binding in &self.bindings {
            if binding.id.peer != op.peer || &binding.container != container {
                continue;
            }
            let mut counter = binding.id.counter;
            for part in &binding.parts {
                let boundaries = part
                    .try_with_bytes("test.ledger", |bytes| -> LoroResult<Vec<usize>> {
                        let value = std::str::from_utf8(bytes)
                            .map_err(|_| invalid("invalid UTF-8 ledger"))?;
                        let mut cuts = value.char_indices().map(|(i, _)| i).collect::<Vec<_>>();
                        cuts.push(bytes.len());
                        Ok(cuts)
                    })
                    .map_err(|_| invalid("missing original extent"))??;
                let finish = counter + (boundaries.len() - 1) as i32;
                let from = counter.max(op.counter);
                let to = finish.min(end);
                if from < to {
                    selected.push((
                        from,
                        to,
                        part.slice_clone(
                            boundaries[(from - counter) as usize]
                                ..boundaries[(to - counter) as usize],
                        ),
                    ));
                }
                counter = finish;
            }
        }
        if selected.is_empty() {
            return Ok(None);
        }
        selected.sort_by_key(|(from, _, _)| *from);
        let mut next = op.counter;
        let mut parts = vec![];
        for (from, to, part) in selected {
            if from != next {
                return Err(invalid("allocation gap/overlap"));
            }
            next = to;
            parts.push(part);
        }
        if next != end || parts.iter().map(TextSlice::len).sum::<usize>() != byte_len {
            return Err(invalid("native span does not match original allocations"));
        }
        Ok(Some(BoundTextExtents {
            provenance: self.provenance,
            parts,
        }))
    }
    fn verify_wire_receipt(&self, wire: &[u8]) -> LoroResult<()> {
        if wire != self.wire {
            return Err(invalid("exact trusted wire receipt mismatch"));
        }
        Ok(())
    }
    fn verify_typed_semantics(&self, wire: &[u8]) -> LoroResult<()> {
        let actual = LoroDoc::new();
        actual.import(wire)?;
        let expected = LoroDoc::new();
        let typed: loro::JsonSchema = serde_json::from_value(self.typed.clone())
            .map_err(|_| invalid("invalid independent typed ledger"))?;
        expected.import_json_updates(typed)?;
        if actual.oplog_vv() != expected.oplog_vv()
            || actual.get_deep_value() != expected.get_deep_value()
        {
            return Err(invalid("independent typed state mismatch"));
        }
        for (&peer, &end) in expected.oplog_vv().iter() {
            for counter in 0..end {
                let span = loro::IdSpan::new(peer, counter, counter + 1);
                let a = serde_json::to_value(actual.export_json_in_id_span(span)).unwrap();
                let b = serde_json::to_value(expected.export_json_in_id_span(span)).unwrap();
                if a != b {
                    return Err(invalid("independent typed native atom mismatch"));
                }
            }
        }
        Ok(())
    }
}
fn fixture() -> (Ledger, Arc<Memory>) {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    doc.set_record_timestamp(false);
    let memory = Arc::new(Memory::default());
    let mut allocator = Allocator {
        memory: memory.clone(),
        context: None,
        allocations: vec![],
    };
    let text = doc.get_text("body");
    text.insert_generated_unicode(0, 4, &mut allocator, |out| {
        out.copy_from_slice("a玉".as_bytes())
    })
    .unwrap();
    text.insert_generated_unicode(2, 5, &mut allocator, |out| {
        out.copy_from_slice("🙂Z".as_bytes())
    })
    .unwrap();
    // Deliberately identical bytes in a non-text value challenge substring-based holes.
    doc.get_map("metadata").insert("label", "a玉🙂Z").unwrap();
    doc.get_list("members").insert(0, "member").unwrap();
    text.mark(0..2, "bold", true).unwrap();
    doc.set_next_commit_timestamp(42);
    doc.set_next_commit_message("a玉🙂Z");
    doc.commit();
    let schema = doc
        .export_json_updates_without_peer_compression(&VersionVector::default(), &doc.oplog_vv());
    let mut bindings = vec![];
    for change in &schema.changes {
        for op in &change.ops {
            if let JsonOpContent::Text(JsonTextOp::Insert { text, .. }) = &op.content {
                let end = op.counter + text.chars().count() as i32;
                let parts = allocator
                    .allocations
                    .iter()
                    .filter(|(c, _)| {
                        c.id.peer == change.id.peer
                            && c.id.counter >= op.counter
                            && c.id.counter < end
                            && c.container == op.container
                    })
                    .map(|(_, s)| s.clone())
                    .collect::<Vec<_>>();
                assert!(
                    !parts.is_empty(),
                    "every generated operand has original allocation evidence"
                );
                bindings.push(Binding {
                    id: ID::new(change.id.peer, op.counter),
                    container: op.container.clone(),
                    parts,
                });
            }
        }
    }
    (
        Ledger {
            wire: doc.export(ExportMode::all_updates()).unwrap(),
            typed: serde_json::to_value(schema).unwrap(),
            bindings,
            provenance: ExtentProvenance::OriginalLocal,
        },
        memory,
    )
}

#[test]
fn original_generated_extents_roundtrip_mixed_wire_without_string_search() {
    let (ledger, _) = fixture();
    let recipe = capture_native_wire_recipe(&ledger.wire, &ledger).unwrap();
    assert!(
        !recipe.holes.is_empty(),
        "supported generated text must not silently fall back"
    );
    assert_eq!(
        recipe.fragments.iter().map(Vec::len).sum::<usize>()
            + recipe.holes.iter().map(|h| h.byte_len).sum::<usize>(),
        ledger.wire.len()
    );
    let ids = recipe
        .holes
        .iter()
        .flat_map(|h| h.extents.iter().map(|e| e.extent))
        .collect::<Vec<_>>();
    assert!(
        ids.contains(&100) && ids.contains(&101),
        "RLE operand must retain both original extent identities"
    );
    assert_eq!(
        recipe.holes.iter().map(|h| h.byte_len).sum::<usize>(),
        9,
        "map/message copies are literal syntax evidence, not text holes"
    );
    let stats = recipe.stats();
    assert_eq!(stats.referenced_extent_bytes, 9);
    assert_eq!(stats.fallback_bytes, 0);
    assert_eq!(stats.fallback_text_bytes, 0);
    assert!(stats.metadata_json_bytes > 0);
    let persisted: NativeWireRecipe =
        serde_json::from_slice(&serde_json::to_vec(&recipe).unwrap()).unwrap();
    assert_eq!(
        reconstruct_native_wire_recipe(&persisted, &ledger).unwrap(),
        ledger.wire
    );
}

#[test]
fn accepted_noncanonical_header_roundtrips_and_no_binding_is_explicit_literal_fallback() {
    let (mut ledger, _) = fixture();
    ledger.wire[4..16].fill(0xa5);
    ledger.verify_typed_semantics(&ledger.wire).unwrap();
    let recipe = capture_native_wire_recipe(&ledger.wire, &ledger).unwrap();
    assert_eq!(
        reconstruct_native_wire_recipe(&recipe, &ledger).unwrap(),
        ledger.wire
    );
    ledger.bindings.clear();
    let fallback = capture_native_wire_recipe(&ledger.wire, &ledger).unwrap();
    assert!(fallback.holes.is_empty());
    let stats = fallback.stats();
    assert_eq!(stats.fallback_bytes, ledger.wire.len());
    assert_eq!(stats.referenced_extent_bytes, 0);
    assert_eq!(stats.fallback_text_bytes, 9);
    assert_eq!(
        fallback.fragments.iter().map(Vec::len).sum::<usize>(),
        ledger.wire.len()
    );
    assert_eq!(
        reconstruct_native_wire_recipe(&fallback, &ledger).unwrap(),
        ledger.wire
    );
}

#[test]
fn forged_hole_syntax_identity_extent_and_provenance_fail_closed() {
    let (ledger, memory) = fixture();
    let recipe = capture_native_wire_recipe(&ledger.wire, &ledger).unwrap();
    assert!(!recipe.holes.is_empty());
    for fault in 0..8 {
        let mut bad = recipe.clone();
        match fault {
            0 => bad.holes[0].offset = 4,
            1 => bad.holes[0].op.counter += 1,
            2 => bad.holes[0].extents[0].start += 1,
            3 => {
                let extent = &mut bad.holes[0].extents[0];
                let bytes = memory
                    .0
                    .write()
                    .unwrap()
                    .get(&extent.extent)
                    .unwrap()
                    .clone();
                memory.0.write().unwrap().insert(999, bytes);
                extent.extent = 999;
            }
            4 => bad.holes.push(bad.holes[0].clone()),
            5 => bad.holes[0].byte_len += 1,
            6 => bad.holes[0].provenance = ExtentProvenance::ReceivedRemote,
            7 => bad.holes[0].container = ContainerID::new_root("wrong", loro::ContainerType::Text),
            _ => unreachable!(),
        }
        assert!(
            reconstruct_native_wire_recipe(&bad, &ledger).is_err(),
            "accepted forged hole {fault}"
        );
    }
}

#[test]
fn malformed_wire_and_semantically_wrong_ledger_never_become_literal_fallback() {
    let (mut ledger, _) = fixture();
    ledger.bindings.clear();
    for bad in [&ledger.wire[..4], &ledger.wire[..ledger.wire.len() / 2]] {
        let mut matching_receipt = ledger.clone();
        matching_receipt.wire = bad.to_vec();
        assert!(capture_native_wire_recipe(bad, &matching_receipt).is_err());
    }
    let mut wrong = ledger.clone();
    wrong.typed = serde_json::json!({});
    assert!(capture_native_wire_recipe(&ledger.wire, &wrong).is_err());
}

#[test]
fn invalid_claimed_binding_and_hidden_fallback_are_rejected() {
    let (ledger, memory) = fixture();
    let recipe = capture_native_wire_recipe(&ledger.wire, &ledger).unwrap();
    let mut hidden = recipe.clone();
    hidden.fragments = vec![ledger.wire.clone()];
    hidden.holes.clear();
    hidden.fallback_text_bytes = 9;
    assert!(
        reconstruct_native_wire_recipe(&hidden, &ledger).is_err(),
        "usable original extent binding cannot silently become fallback"
    );
    struct EmptyBinding<'a>(&'a Ledger);
    impl NativeWireLedger for EmptyBinding<'_> {
        fn text_extents(
            &self,
            _: ID,
            _: &ContainerID,
            _: u32,
            _: usize,
        ) -> LoroResult<Option<BoundTextExtents>> {
            Ok(Some(BoundTextExtents {
                provenance: ExtentProvenance::OriginalLocal,
                parts: vec![],
            }))
        }
        fn verify_wire_receipt(&self, wire: &[u8]) -> LoroResult<()> {
            self.0.verify_wire_receipt(wire)
        }
        fn verify_typed_semantics(&self, wire: &[u8]) -> LoroResult<()> {
            self.0.verify_typed_semantics(wire)
        }
    }
    assert!(capture_native_wire_recipe(&ledger.wire, &EmptyBinding(&ledger)).is_err());
    memory.0.write().unwrap().get_mut(&100).unwrap()[0] = b'x';
    assert!(
        capture_native_wire_recipe(&ledger.wire, &ledger).is_err(),
        "claimed original bytes differ from accepted operand"
    );
}

#[test]
#[ignore = "requires independently admitted native syntax fixtures"]
fn admitted_duplicate_blocks_and_overlong_lengths_keep_exact_wire() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LORO_RECIPE_FIXTURE_DIR").expect("fixture directory required"),
    );
    let memory = Arc::new(Memory::default());
    memory
        .0
        .write()
        .unwrap()
        .insert(901, "original玉".as_bytes().to_vec());
    for (canonical, variant, text) in [
        (
            "single-text-block-update.bin",
            "duplicate-text-block-update.bin",
            true,
        ),
        (
            "canonical-update.bin",
            "overlong-block-length-update.bin",
            false,
        ),
    ] {
        let original = std::fs::read(root.join(canonical)).unwrap();
        let wire = std::fs::read(root.join(variant)).unwrap();
        let doc = LoroDoc::new();
        doc.import(&original).unwrap();
        let ledger = Ledger {
            wire: wire.clone(),
            typed: serde_json::to_value(doc.export_json_updates_without_peer_compression(
                &VersionVector::default(),
                &doc.oplog_vv(),
            ))
            .unwrap(),
            bindings: if text {
                vec![Binding {
                    id: ID::new(42, 0),
                    container: ContainerID::new_root("receipt", loro::ContainerType::Text),
                    parts: vec![TextSlice::extent(
                        memory.clone(),
                        901,
                        0.."original玉".len(),
                    )],
                }]
            } else {
                vec![]
            },
            provenance: ExtentProvenance::ReceivedRemote,
        };
        let recipe = capture_native_wire_recipe(&wire, &ledger).unwrap();
        assert_eq!(
            reconstruct_native_wire_recipe(&recipe, &ledger).unwrap(),
            wire
        );
        if text {
            assert_eq!(
                recipe.holes.len(),
                2,
                "two physical occurrences of one native operand"
            );
            assert_eq!(
                recipe.stats().referenced_extent_bytes,
                2 * "original玉".len()
            );
            assert_eq!(
                recipe.stats().unique_referenced_extent_bytes,
                "original玉".len()
            );
        } else {
            assert!(recipe.holes.is_empty());
            assert_eq!(recipe.stats().fallback_bytes, wire.len());
        }
    }
}

#[test]
#[ignore = "requires independently admitted native syntax fixtures"]
fn admitted_full_prefix_suffix_blocks_bind_scalar_subranges_of_original_extent() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LORO_RECIPE_FIXTURE_DIR").expect("fixture directory required"),
    );
    let original = std::fs::read(root.join("full-unicode-text-update.bin")).unwrap();
    let wire = std::fs::read(root.join("overlapping-unicode-text-cuts-update.bin")).unwrap();
    let doc = LoroDoc::new();
    doc.import(&original).unwrap();
    let memory = Arc::new(Memory::default());
    memory
        .0
        .write()
        .unwrap()
        .insert(902, "a玉🙂Z".as_bytes().to_vec());
    let ledger = Ledger {
        wire: wire.clone(),
        typed: serde_json::to_value(doc.export_json_updates_without_peer_compression(
            &VersionVector::default(),
            &doc.oplog_vv(),
        ))
        .unwrap(),
        bindings: vec![Binding {
            id: ID::new(42, 0),
            container: ContainerID::new_root("receipt", loro::ContainerType::Text),
            parts: vec![TextSlice::extent(memory, 902, 0..9)],
        }],
        provenance: ExtentProvenance::ReceivedRemote,
    };
    let recipe = capture_native_wire_recipe(&wire, &ledger).unwrap();
    assert_eq!(recipe.holes.len(), 3);
    assert_eq!(recipe.stats().referenced_extent_bytes, 18);
    assert_eq!(recipe.stats().unique_referenced_extent_bytes, 9);
    assert_eq!(
        recipe
            .holes
            .iter()
            .map(|h| (
                h.op.counter,
                h.unicode_len,
                h.extents[0].start,
                h.extents[0].end
            ))
            .collect::<Vec<_>>(),
        vec![(0, 4, 0, 9), (0, 2, 0, 4), (2, 2, 4, 9)]
    );
    assert_eq!(
        reconstruct_native_wire_recipe(&recipe, &ledger).unwrap(),
        wire
    );
}

#[test]
fn strict_recipe_uses_native_reference_decoder_and_span_scanner() {
    use loro::native_reference_oracle::NativeTextLookup;
    use loro::native_wire_recipe::{capture_native_wire_recipe_with_lookup, reconstruct_native_wire_recipe_with_lookup};
    struct Lookup<'a>(&'a Ledger);
    impl NativeTextLookup for Lookup<'_> {
        fn text_extents(&self, op: ID, cid: &ContainerID, unicode_len: u32, byte_len: usize) -> LoroResult<Option<BoundTextExtents>> {
            self.0.text_extents(op, cid, unicode_len, byte_len)
        }
    }
    let (ledger, _) = fixture();
    let lookup = Lookup(&ledger);
    let recipe = capture_native_wire_recipe_with_lookup(&ledger.wire, &ledger, &lookup).unwrap();
    assert_eq!(reconstruct_native_wire_recipe_with_lookup(&recipe, &ledger, &lookup).unwrap(), ledger.wire);
    struct Missing;
    impl NativeTextLookup for Missing {
        fn text_extents(&self, _: ID, _: &ContainerID, _: u32, _: usize) -> LoroResult<Option<BoundTextExtents>> { Ok(None) }
    }
    assert!(capture_native_wire_recipe_with_lookup(&ledger.wire, &ledger, &Missing).is_err());
}
