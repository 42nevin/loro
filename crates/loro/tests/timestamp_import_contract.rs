//! Metadata is preserved when adjacent changes cannot be safely merged.
use loro::{ExportMode, LoroDoc, VersionVector};
fn schema(doc: &LoroDoc) -> loro::JsonSchema {
    doc.export_json_updates_without_peer_compression(&VersionVector::default(), &doc.oplog_vv())
}
#[test]
#[ignore = "explicit actual-app admitted fixture lane; requires GENESIS_COMPAT_FIXTURE"]
fn actually_admitted_genesis_metadata_imports_without_normalization() {
    let directory = std::path::PathBuf::from(std::env::var("GENESIS_COMPAT_FIXTURE").unwrap());
    let expected = LoroDoc::new();
    expected
        .import(&std::fs::read(directory.join("mutated.snapshot")).unwrap())
        .unwrap();
    let original: loro::JsonSchema =
        serde_json::from_slice(&std::fs::read(directory.join("app-admitted-schema.json")).unwrap())
            .unwrap();
    let actual = LoroDoc::new();
    let status = actual.import_json_updates(original).unwrap();
    assert!(status.pending.is_none());
    assert_eq!(actual.oplog_vv(), expected.oplog_vv());
    assert_eq!(actual.state_vv(), actual.oplog_vv());
    assert_eq!(actual.get_deep_value(), expected.get_deep_value());
    for (&peer, &end) in expected.oplog_vv().iter() {
        for counter in 0..end {
            let span = loro::IdSpan::new(peer, counter, counter + 1);
            assert_eq!(
                serde_json::to_value(actual.export_json_in_id_span(span)).unwrap(),
                serde_json::to_value(expected.export_json_in_id_span(span)).unwrap()
            );
        }
    }
    actual.set_peer_id(991).unwrap();
    actual.set_next_commit_timestamp(30);
    actual.get_map("next").insert("safe", true).unwrap();
    actual.commit();
    let cold = LoroDoc::new();
    cold.import(&actual.export(ExportMode::snapshot()).unwrap())
        .unwrap();
    assert_eq!(cold.get_deep_value(), actual.get_deep_value());
    println!("ADMITTED_GENESIS_TYPED_IMPORT_COMPLETE");
}
#[test]
fn timestamp_extremes_and_nonmergeable_messages_preserve_metadata() {
    for (first, last, same_message) in [
        (i64::MIN, i64::MAX, true),
        (i64::MAX, i64::MIN, true),
        (20, 10, true),
        (20, 10, false),
        (10, 20, false),
        (20, 21, true),
    ] {
        let source = LoroDoc::new();
        source.set_peer_id(42).unwrap();
        source.set_next_commit_timestamp(10);
        source.set_next_commit_message("one");
        source.get_map("map").insert("a", 1).unwrap();
        source.commit();
        source.set_next_commit_timestamp(20);
        source.set_next_commit_message("two");
        source.get_map("map").insert("b", 2).unwrap();
        source.commit();
        let mut input = schema(&source);
        assert_eq!(input.changes.len(), 2);
        input.changes[0].timestamp = first;
        input.changes[1].timestamp = last;
        if same_message {
            input.changes[1].msg = input.changes[0].msg.clone();
        }
        let expected = serde_json::to_value(&input).unwrap();
        let target = LoroDoc::new();
        let status = target.import_json_updates(input).unwrap();
        assert!(status.pending.is_none());
        assert_eq!(
            serde_json::to_value(schema(&target)).unwrap(),
            expected,
            "metadata altered for {first}->{last}"
        );
        assert_eq!(target.get_deep_value(), source.get_deep_value());
    }
}
#[test]
fn ordinary_equal_and_increasing_timestamp_merge_semantics_remain_valid() {
    // Distinct concurrent peers cannot be coalesced merely because metadata matches.
    let left = LoroDoc::new();
    left.set_peer_id(101).unwrap();
    left.set_next_commit_timestamp(20);
    left.set_next_commit_message("same");
    left.get_map("map").insert("left", 1).unwrap();
    left.commit();
    let right = LoroDoc::new();
    right.set_peer_id(102).unwrap();
    right.set_next_commit_timestamp(10);
    right.set_next_commit_message("same");
    right.get_map("map").insert("right", 2).unwrap();
    right.commit();
    left.import(&right.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    let merged = LoroDoc::new();
    merged.import_json_updates(schema(&left)).unwrap();
    assert_eq!(merged.oplog_vv(), left.oplog_vv());
    assert_eq!(merged.get_deep_value(), left.get_deep_value());
    assert_eq!(
        serde_json::to_value(schema(&merged)).unwrap(),
        serde_json::to_value(schema(&left)).unwrap()
    );
    for (first, last) in [(10, 10), (10, 20)] {
        let source = LoroDoc::new();
        source.set_peer_id(42).unwrap();
        source.set_next_commit_timestamp(first);
        source.set_next_commit_message("same");
        source.get_map("map").insert("a", 1).unwrap();
        source.commit();
        source.set_next_commit_timestamp(last);
        source.set_next_commit_message("same");
        source.get_map("map").insert("b", 2).unwrap();
        source.commit();
        let target = LoroDoc::new();
        target.import_json_updates(schema(&source)).unwrap();
        assert_eq!(target.oplog_vv(), source.oplog_vv());
        assert_eq!(target.get_deep_value(), source.get_deep_value());
        assert_eq!(
            serde_json::to_value(schema(&target)).unwrap(),
            serde_json::to_value(schema(&source)).unwrap()
        );
    }
}
