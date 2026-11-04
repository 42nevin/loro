#![cfg(feature = "shared_arena_experiment")]
use append_only_bytes::BytesSlice;
use loro::{ExportMode, LoroDoc, TextExtentAllocator};

// Deliberately models a reservation whose guard expires. Generation writes into
// the reservation first; the mandatory native-owner copy is separately counted.
#[derive(Default)]
struct BoundedReservation {
    copies: usize,
    generated: usize,
}
impl TextExtentAllocator for BoundedReservation {
    fn allocate(
        &mut self,
        len: usize,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> std::io::Result<BytesSlice> {
        let mut reservation = vec![0; len];
        f(&mut reservation);
        self.generated += len;
        let native = BytesSlice::from_bytes(&reservation);
        self.copies += len;
        reservation.fill(0xdd); // retained Op may not depend on this reservation
        Ok(native)
    }
}
fn generated(doc: &LoroDoc, pos: usize, chars: &[char], allocator: &mut BoundedReservation) {
    let bytes = chars.iter().map(|c| c.len_utf8()).sum();
    doc.get_text("text")
        .insert_generated_unicode(pos, bytes, allocator, |dst| {
            let mut offset = 0;
            for c in chars {
                let n = c.len_utf8();
                c.encode_utf8(&mut dst[offset..offset + n]);
                offset += n;
            }
        })
        .unwrap();
}
#[test]
fn generated_gestum_cuts_survive_guard_drop_reopen_and_native_update() {
    let mut allocator = BoundedReservation::default();
    let g1 = LoroDoc::new();
    g1.set_peer_id(7).unwrap();
    generated(&g1, 0, &['a'], &mut allocator);
    g1.commit();
    let cut = g1.oplog_frontiers();
    let vv = g1.oplog_vv();
    let snapshot = g1.export(ExportMode::Snapshot).unwrap();
    drop(g1);
    let g2 = LoroDoc::new();
    g2.import(&snapshot).unwrap();
    g2.set_peer_id(7).unwrap();
    generated(&g2, 1, &['玉'], &mut allocator);
    g2.commit();
    assert_eq!(g2.oplog_vv().get(&7), Some(&2));
    let peer = LoroDoc::new();
    peer.import(&snapshot).unwrap();
    peer.import(&g2.export(ExportMode::updates(&vv)).unwrap())
        .unwrap();
    assert_eq!(peer.get_text("text").to_string(), "a玉");
    assert_eq!(peer.oplog_vv().get(&7), Some(&2));
    let decoded = peer.export_json_updates(&vv, &peer.oplog_vv());
    assert_eq!(decoded.changes.len(), 1);
    assert_eq!(decoded.changes[0].op_len(), 1);
    assert_eq!(decoded.changes[0].ops.len(), 1);
    assert_eq!(decoded.changes[0].ops[0].counter, 1);
    let operand = serde_json::to_value(&decoded.changes[0].ops[0]).unwrap();
    assert_eq!(operand["content"]["text"], "玉");
    assert_eq!(operand["content"]["pos"], 1);

    g2.checkout(&cut).unwrap();
    assert_eq!(g2.get_text("text").to_string(), "a");
    assert_eq!(allocator.generated, 4);
    assert_eq!(allocator.copies, 4);
}
#[test]
fn tiny_extents_unicode_middle_edits_cow_and_concurrent_native_sync() {
    let source = LoroDoc::new();
    source.set_peer_id(11).unwrap();
    let mut allocator = BoundedReservation::default();
    for (i, c) in ['a', '界', '🙂', 'e', '\u{301}', 'Z']
        .into_iter()
        .enumerate()
    {
        generated(&source, i, &[c], &mut allocator);
    }
    source.commit();
    let original = source.get_text("text").to_string();
    let reference = LoroDoc::new();
    reference.set_peer_id(11).unwrap();
    reference.get_text("text").insert(0, &original).unwrap();
    reference.commit();
    let expand = |doc: &LoroDoc| {
        let schema = doc.export_json_updates(&Default::default(), &doc.oplog_vv());
        let mut atoms = Vec::new();
        let mut runs = 0;
        for change in schema.changes {
            for op in change.ops {
                runs += 1;
                let value = serde_json::to_value(&op).unwrap();
                let pos = value["content"]["pos"].as_u64().unwrap();
                for (offset, ch) in value["content"]["text"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .enumerate()
                {
                    atoms.push((op.counter + offset as i32, pos + offset as u64, ch));
                }
            }
        }
        (runs, atoms)
    };
    let (split_runs, split_atoms) = expand(&source);
    let (packed_runs, packed_atoms) = expand(&reference);
    assert_eq!(split_runs, 6);
    assert_eq!(packed_runs, 1);
    assert_eq!(split_atoms, packed_atoms);

    let left = source.fork_cow().unwrap();
    left.set_peer_id(12).unwrap();
    let right = source.fork_cow().unwrap();
    right.set_peer_id(13).unwrap();
    let left_thread = std::thread::spawn(move || {
        let mut allocator = BoundedReservation::default();
        generated(&left, 2, &['玉'], &mut allocator);
        left.get_text("text").delete(4, 1).unwrap();
        left.commit();
        left
    });
    let right_thread = std::thread::spawn(move || {
        let mut allocator = BoundedReservation::default();
        generated(&right, 1, &['中'], &mut allocator);
        right.commit();
        right
    });
    let left = left_thread.join().unwrap();
    let right = right_thread.join().unwrap();
    let loser = source.fork_cow().unwrap();
    generated(&loser, 3, &['X'], &mut allocator);
    loser.commit();
    drop(loser);
    assert_eq!(source.get_text("text").to_string(), original);
    left.import(&right.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    right
        .import(&left.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    assert_eq!(left.get_deep_value(), right.get_deep_value());
    assert_eq!(left.oplog_vv(), right.oplog_vv());
    let reopen = LoroDoc::new();
    reopen
        .import(&left.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    assert_eq!(reopen.get_deep_value(), left.get_deep_value());
}
#[test]
fn invalid_generated_utf8_does_not_publish_an_atom() {
    let doc = LoroDoc::new();
    let before = doc.oplog_vv();
    let mut allocator = BoundedReservation::default();
    assert!(doc
        .get_text("text")
        .insert_generated_unicode(0, 1, &mut allocator, |dst| dst[0] = 0xff)
        .is_err());
    assert_eq!(doc.oplog_vv(), before);
    assert_eq!(doc.get_text("text").to_string(), "");
    generated(&doc, 0, &['玉'], &mut allocator);
    doc.commit();
    assert_eq!(doc.get_text("text").to_string(), "玉");
}

#[test]
fn allocation_error_and_generator_panic_leave_no_atom_or_alias() {
    struct Failing;
    impl TextExtentAllocator for Failing {
        fn allocate(
            &mut self,
            _: usize,
            _: &mut dyn FnMut(&mut [u8]),
        ) -> std::io::Result<BytesSlice> {
            Err(std::io::Error::other("reservation failed"))
        }
    }
    let doc = LoroDoc::new();
    let text = doc.get_text("text");
    let before = doc.oplog_vv();
    assert!(text
        .insert_generated_unicode(0, 1, &mut Failing, |_| panic!("must not run"))
        .is_err());
    assert_eq!(doc.oplog_vv(), before);
    let mut allocator = BoundedReservation::default();
    assert!(text
        .insert_generated_unicode(0, 3, &mut allocator, |dst| {
            dst[0] = b'x';
            panic!("generation aborted");
        })
        .is_err());
    assert_eq!(doc.oplog_vv(), before);
    assert_eq!(text.to_string(), "");
    generated(&doc, 0, &['玉'], &mut allocator);
    doc.commit();
    assert_eq!(text.to_string(), "玉");
}

#[test]
fn cleared_style_history_is_rejected_before_generation_if_anchors_remain() {
    let doc = LoroDoc::new();
    let text = doc.get_text("text");
    text.insert(0, "abc").unwrap();
    text.mark(0..2, "bold", true).unwrap();
    text.unmark(0..2, "bold").unwrap();
    doc.commit();
    let before = doc.oplog_vv();
    let mut allocator = BoundedReservation::default();
    assert!(text
        .insert_generated_unicode(1, 1, &mut allocator, |b| b[0] = b'x')
        .is_err());
    assert_eq!(allocator.generated, 0);
    assert_eq!(doc.oplog_vv(), before);
    assert_eq!(text.to_string(), "abc");
}

#[test]
fn allocation_context_is_actual_and_rejection_precedes_generation() {
    use loro::{ContainerTrait, Frontiers, TextAllocationContext, ID};
    #[derive(Default)]
    struct CheckedAllocator {
        expected: Option<TextAllocationContext>,
        seen: Vec<TextAllocationContext>,
        allocated: usize,
    }
    impl TextExtentAllocator for CheckedAllocator {
        fn begin_allocation(&mut self, context: &TextAllocationContext) -> std::io::Result<()> {
            if self
                .expected
                .as_ref()
                .is_some_and(|expected| expected != context)
            {
                return Err(std::io::Error::other("native allocation operands differ"));
            }
            self.seen.push(context.clone());
            Ok(())
        }
        fn allocate(
            &mut self,
            len: usize,
            generate: &mut dyn FnMut(&mut [u8]),
        ) -> std::io::Result<BytesSlice> {
            self.allocated += 1;
            let mut bytes = vec![0; len];
            generate(&mut bytes);
            Ok(BytesSlice::from_bytes(&bytes))
        }
    }
    let doc = LoroDoc::new();
    doc.set_peer_id(10).unwrap();
    doc.get_text("text").insert(0, "a").unwrap();
    doc.commit();
    let left = doc.fork_cow().unwrap();
    left.set_peer_id(11).unwrap();
    left.get_text("text").insert(1, "b").unwrap();
    left.commit();
    let right = doc.fork_cow().unwrap();
    right.set_peer_id(12).unwrap();
    right.get_text("text").insert(1, "c").unwrap();
    right.commit();
    doc.import(&left.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    doc.import(&right.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    let deps = doc.oplog_frontiers();
    assert_eq!(deps.len(), 2);
    let vv = doc.oplog_vv();
    let expected = TextAllocationContext {
        id: ID::new(10, 1),
        container: doc.get_text("text").id(),
        unicode_pos: 1,
        entity_pos: 1,
        lamport: 2,
        deps: deps.clone(),
    };
    for wrong_deps in [false, true] {
        let mut wrong = expected.clone();
        if wrong_deps {
            wrong.deps = Frontiers::default();
        } else {
            wrong.id.counter = 99;
        }
        let mut allocator = CheckedAllocator {
            expected: Some(wrong),
            ..Default::default()
        };
        assert!(doc
            .get_text("text")
            .insert_generated_unicode(1, 1, &mut allocator, |_| {
                panic!("rejected metadata must not generate payload")
            })
            .is_err());
        assert_eq!(allocator.allocated, 0);
        assert!(allocator.seen.is_empty());
        assert_eq!(doc.oplog_vv(), vv);
    }
    let mut allocator = CheckedAllocator {
        expected: Some(expected.clone()),
        ..Default::default()
    };
    doc.get_text("text")
        .insert_generated_unicode(1, 1, &mut allocator, |b| b[0] = b'x')
        .unwrap();
    assert_eq!(allocator.seen, vec![expected]);
    allocator.expected = Some(TextAllocationContext {
        id: ID::new(10, 2),
        container: doc.get_text("text").id(),
        unicode_pos: 2,
        entity_pos: 2,
        lamport: 3,
        deps: Frontiers::from_id(ID::new(10, 1)),
    });
    doc.get_text("text")
        .insert_generated_unicode(2, 1, &mut allocator, |b| b[0] = b'y')
        .unwrap();
    assert_eq!(allocator.allocated, 2);
    doc.commit();
    let decoded = doc.export_json_updates(&vv, &doc.oplog_vv());
    assert_eq!(decoded.changes.len(), 1);
    assert_eq!(decoded.changes[0].lamport, 2);
    assert_eq!(decoded.changes[0].ops[0].counter, 1);
    assert_eq!(decoded.changes[0].op_len(), 2);
}
