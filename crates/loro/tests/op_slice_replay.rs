#![cfg(feature = "shared_arena_experiment")]
use loro::{
    ContainerID, ContainerType, ExportMode, ExtentChangeDescriptor, ExtentOpDescriptor,
    ExtentResolver, ExtentTextOp, LoroDoc, TextSlice, VersionVector, ID,
};
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

const PEER: u64 = 71;
const PIECES: &[&str] = &[
    "a",
    "é界🙂e\u{301}Z",
    "玉",
    "👩\u{200d}💻xy",
    "!",
    "尾🙂abc",
];
#[derive(Default)]
struct Resolver {
    payloads: Vec<Vec<u8>>,
    reads: Mutex<Vec<&'static str>>,
}
impl ExtentResolver for Resolver {
    fn with_bytes(
        &self,
        extent: u64,
        range: Range<usize>,
        consumer: &'static str,
        visit: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<()> {
        self.reads.lock().unwrap().push(consumer);
        visit(&self.payloads[extent as usize][range]);
        Ok(())
    }
}
fn descriptors(logical: bool) -> (Vec<ExtentChangeDescriptor>, Arc<Resolver>) {
    let resolver = Arc::new(Resolver {
        payloads: PIECES
            .iter()
            .map(|s| format!("!!{s}??").into_bytes())
            .collect(),
        ..Resolver::default()
    });
    let mut counter = 0;
    let ops = PIECES
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let op = ExtentOpDescriptor {
                counter,
                container: ContainerID::new_root("text", ContainerType::Text),
                content: ExtentTextOp::Insert {
                    pos: counter as u32,
                    text: if logical {
                        TextSlice::extent(resolver.clone(), i as u64, 2..2 + text.len())
                    } else {
                        TextSlice::from_bytes(text.as_bytes())
                    },
                },
            };
            counter += text.chars().count() as i32;
            op
        })
        .collect();
    (
        vec![ExtentChangeDescriptor {
            id: ID::new(PEER, 0),
            lamport: 0,
            timestamp: 0,
            deps: Default::default(),
            message: None,
            ops,
        }],
        resolver,
    )
}
fn recovered(logical: bool) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.import_extent_descriptors(descriptors(logical).0)
        .unwrap();
    doc
}
fn same(a: &LoroDoc, b: &LoroDoc) {
    assert_eq!(a.get_deep_value(), b.get_deep_value());
    assert_eq!(a.oplog_vv(), b.oplog_vv());
    assert_eq!(a.state_frontiers(), b.state_frontiers());
}
fn atoms(doc: &LoroDoc) -> Vec<(u64, i32, u64, char)> {
    let mut atoms = Vec::new();
    for change in doc
        .export_json_updates(&Default::default(), &doc.oplog_vv())
        .changes
    {
        for op in change.ops {
            let value = serde_json::to_value(&op).unwrap();
            let pos = value["content"]["pos"].as_u64().unwrap();
            for (i, ch) in value["content"]["text"]
                .as_str()
                .unwrap()
                .chars()
                .enumerate()
            {
                atoms.push((change.id.peer, op.counter + i as i32, pos + i as u64, ch));
            }
        }
    }
    atoms
}

#[test]
fn partial_multiscalar_history_and_incremental_suffix_preserve_atom_identity() {
    let text = PIECES.concat();
    let packed = LoroDoc::new();
    packed.set_peer_id(PEER).unwrap();
    let mut position = 0;
    for piece in PIECES {
        packed.get_text("text").insert(position, piece).unwrap();
        position += piece.chars().count();
    }
    packed.commit();
    let packed_history = packed.export_json_updates(&Default::default(), &packed.oplog_vv());
    // Native allocation boundaries can leave more than one run. Require the
    // run containing the cut to span an original insertion boundary at scalar 7.
    assert!(
        packed_history
            .changes
            .iter()
            .flat_map(|change| &change.ops)
            .any(|op| {
                let value = serde_json::to_value(op).unwrap();
                let end =
                    op.counter + value["content"]["text"].as_str().unwrap().chars().count() as i32;
                op.counter < 5 && end > 7
            }),
        "cut must be inside a run merged from multiple adjacent insertions"
    );
    let mut packed_mid = VersionVector::default();
    packed_mid.set_end(ID::new(PEER, 5));
    let packed_partial = LoroDoc::new();
    packed_partial
        .import(
            &packed
                .export(ExportMode::updates_till(&packed_mid))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        packed_partial.get_text("text").to_string(),
        text.chars().take(5).collect::<String>()
    );
    packed_partial
        .import(&packed.export(ExportMode::updates(&packed_mid)).unwrap())
        .unwrap();
    same(&packed_partial, &packed);
    assert_eq!(atoms(&packed_partial), atoms(&packed));
    for logical in [false, true] {
        let doc = recovered(logical);
        same(&doc, &packed);
        assert_eq!(atoms(&doc), atoms(&packed));
        let cut = 5; // Inside PIECES[1], not at an extent or op edge.
        let mut mid = VersionVector::default();
        mid.set_end(ID::new(PEER, cut));
        let partial = LoroDoc::new();
        partial
            .import(&doc.export(ExportMode::updates_till(&mid)).unwrap())
            .unwrap();
        assert_eq!(
            partial.get_text("text").to_string(),
            text.chars().take(cut as usize).collect::<String>()
        );
        assert_eq!(partial.oplog_vv(), mid);
        doc.checkout(&[ID::new(PEER, cut - 1)].into_iter().collect())
            .unwrap();
        assert_eq!(
            doc.get_text("text").to_string(),
            partial.get_text("text").to_string()
        );
        doc.checkout_to_latest();
        // Only the suffix is supplied: RichOp begins inside an existing run.
        partial
            .import(&doc.export(ExportMode::updates(&mid)).unwrap())
            .unwrap();
        same(&partial, &doc);
        assert_eq!(atoms(&partial), atoms(&packed));
        // Full-history overlap also has to discard the already known prefix.
        let overlap = LoroDoc::new();
        overlap
            .import(&doc.export(ExportMode::updates_till(&mid)).unwrap())
            .unwrap();
        overlap
            .import_extent_descriptors(descriptors(logical).0)
            .unwrap();
        same(&overlap, &doc);
        assert_eq!(atoms(&overlap), atoms(&packed));
    }
}

#[test]
fn replay_middle_edits_concurrent_forks_and_roundtrips_converge() {
    for logical in [false, true] {
        let base = recovered(logical);
        let left = base.fork_cow().unwrap();
        let right = base.fork_cow().unwrap();
        left.set_peer_id(81).unwrap();
        right.set_peer_id(82).unwrap();
        left.get_text("text").insert(3, "中🙂").unwrap();
        left.get_text("text").delete(7, 2).unwrap();
        left.commit();
        right.get_text("text").delete(2, 3).unwrap();
        right.get_text("text").insert(2, "e\u{301}尾").unwrap();
        right.commit();
        let l = left.export(ExportMode::updates(&base.oplog_vv())).unwrap();
        let r = right.export(ExportMode::updates(&base.oplog_vv())).unwrap();
        left.import(&r).unwrap();
        right.import(&l).unwrap();
        same(&left, &right);
        assert_eq!(base.get_text("text").to_string(), PIECES.concat());
        let wire = LoroDoc::new();
        wire.import(&left.export(ExportMode::all_updates()).unwrap())
            .unwrap();
        same(&wire, &left);
        let snapshot = LoroDoc::from_snapshot(&left.export(ExportMode::Snapshot).unwrap()).unwrap();
        same(&snapshot, &left);
        snapshot.checkout(&base.state_frontiers()).unwrap();
        assert_eq!(snapshot.get_text("text").to_string(), PIECES.concat());
        snapshot.checkout_to_latest();
        same(&snapshot, &left);
    }
}

#[test]
fn linear_descriptor_replay_uses_owned_operand_without_arena_reresolution() {
    let (changes, resolver) = descriptors(true);
    let doc = LoroDoc::new();
    loro::reset_text_copy_metrics();
    doc.import_extent_descriptors(changes).unwrap();
    // Snapshot diagnostics before any export/string/history oracle reads.
    let reads = resolver.reads.lock().unwrap().clone();
    let copies = loro::text_copy_metrics();
    assert!(
        !reads
            .iter()
            .any(|consumer| *consumer == "arena.unicode_index"
                || *consumer == "arena.cross_extent_join"),
        "linear import re-resolved owned operand through arena: {reads:?}"
    );
    assert!(
        !copies.contains_key("arena.cross_extent_join"),
        "linear import joined arena interval: {copies:?}"
    );
    assert_eq!(doc.get_text("text").to_string(), PIECES.concat());
}

#[test]
fn native_styled_unicode_replay_preserves_marks_and_survives_source_drop() {
    let source = LoroDoc::new();
    source.set_peer_id(91).unwrap();
    let text = source.get_text("text");
    text.insert(0, "A界🙂e\u{301}尾Z").unwrap();
    text.mark(1..5, "bold", true).unwrap();
    source.commit();
    let target = LoroDoc::new();
    target
        .import(&source.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    let known = target.oplog_vv();
    text.unmark(2..4, "bold").unwrap();
    text.insert(3, "玉👩\u{200d}💻").unwrap();
    text.delete(1, 1).unwrap();
    text.mark(2..6, "italic", true).unwrap();
    source.commit();
    target
        .import(&source.export(ExportMode::updates(&known)).unwrap())
        .unwrap();
    same(&source, &target);
    let expected_delta = source.get_text("text").to_delta();
    assert_eq!(target.get_text("text").to_delta(), expected_delta);
    drop(text);
    drop(source);
    assert_eq!(target.get_text("text").to_delta(), expected_delta);
    let reopened = LoroDoc::from_snapshot(&target.export(ExportMode::Snapshot).unwrap()).unwrap();
    same(&reopened, &target);
    assert_eq!(reopened.get_text("text").to_delta(), expected_delta);
}
