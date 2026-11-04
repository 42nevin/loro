// Independent differential helpers, prepared before E/R implementation.
// These helpers intentionally compare per-native-operation causal/content evidence,
// not only the materialized state or whole-change grouping.
use loro::{CommitOptions, ExportMode, IdSpan, JsonChange, LoroDoc, VersionVector};
use serde_json::Value;

pub fn load(bytes: &[u8], peer: u64) -> LoroDoc {
    let doc = LoroDoc::new();
    let status = doc.import(bytes).unwrap();
    assert!(status.pending.is_none());
    doc.set_peer_id(peer).unwrap();
    doc
}

pub fn snapshot(doc: &LoroDoc) -> Vec<u8> {
    doc.commit();
    doc.export(ExportMode::snapshot()).unwrap()
}

pub fn native_atom(doc: &LoroDoc, peer: u64, counter: i32) -> Value {
    let mut changes = doc.export_json_in_id_span(IdSpan::new(peer, counter, counter + 1));
    assert_eq!(changes.len(), 1, "missing atom {counter}@{peer}");
    normalize_changes(&mut changes)
}

pub fn normalize_changes(changes: &mut [JsonChange]) -> Value {
    for change in changes.iter_mut() {
        change.deps.sort_unstable_by_key(|id| (id.peer, id.counter));
        // Loro's singleton reverse delete is equivalent to its forward singleton.
        // Preserve all larger deletion run directions and every other payload field.
        for op in &mut change.ops {
            match &mut op.content {
                loro::JsonOpContent::Text(loro::JsonTextOp::Delete { len, .. })
                | loro::JsonOpContent::List(loro::JsonListOp::Delete { len, .. })
                | loro::JsonOpContent::MovableList(loro::JsonMovableListOp::Delete {
                    len, ..
                }) => {
                    if *len == -1 {
                        *len = 1;
                    }
                }
                _ => {}
            }
        }
    }
    serde_json::to_value(changes).unwrap()
}

pub fn assert_native_equal(actual: &LoroDoc, expected: &LoroDoc) {
    assert_eq!(actual.oplog_vv(), expected.oplog_vv(), "native VV");
    assert_eq!(actual.state_vv(), expected.state_vv(), "state VV");
    assert_eq!(
        actual.oplog_frontiers(),
        expected.oplog_frontiers(),
        "causal frontiers"
    );
    assert_eq!(
        actual.get_deep_value(),
        expected.get_deep_value(),
        "materialized state"
    );
    for (&peer, &end) in expected.oplog_vv().iter() {
        for counter in 0..end {
            assert_eq!(
                native_atom(actual, peer, counter),
                native_atom(expected, peer, counter),
                "native op/dependencies {counter}@{peer}"
            );
        }
    }
}

pub fn commit(doc: &LoroDoc, n: usize) {
    doc.commit_with(
        CommitOptions::new()
            .timestamp(n as i64)
            .commit_msg(&format!("independent-{n}")),
    );
}

pub fn map_history(peer: u64, changes: usize) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(peer).unwrap();
    doc.set_change_merge_interval(0);
    doc.set_record_timestamp(true);
    let map = doc.get_map("history");
    for i in 0..changes {
        let payload: String = (0..192)
            .map(|j| {
                char::from_u32(0x20 + ((i * 7919 + j * 101 + (i ^ j) * 37) % 90) as u32).unwrap()
            })
            .collect();
        map.insert(&format!("key-{i}"), payload).unwrap();
        commit(&doc, i);
    }
    doc
}

pub fn import_two_ranges_before_prerequisites(first: &[u8], second: &[u8], complete: &LoroDoc) {
    let target = LoroDoc::new();
    let status = target.import(second).unwrap();
    assert!(
        status.pending.is_some(),
        "missing dependencies must remain pending"
    );
    assert!(target.oplog_vv().is_empty());
    let status = target.import(first).unwrap();
    assert!(status.pending.is_none());
    assert_native_equal(&target, complete);
    let duplicate = target.import(second).unwrap();
    assert!(duplicate.success.is_empty());
    assert!(duplicate.pending.is_none());
    assert_native_equal(&target, complete);
}

pub fn assert_invalid_import_atomic(doc: &LoroDoc, bytes: &[u8]) {
    let state = doc.get_deep_value();
    let vv = doc.oplog_vv();
    let frontiers = doc.oplog_frontiers();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| doc.import(bytes)));
    assert!(
        result.is_ok(),
        "invalid external input must return Err, not panic"
    );
    assert!(result.unwrap().is_err(), "malformed input was accepted");
    assert_eq!(doc.get_deep_value(), state);
    assert_eq!(doc.oplog_vv(), vv);
    assert_eq!(doc.oplog_frontiers(), frontiers);
    // A successful ordinary edit demonstrates that rollback did not leave the doc unusable.
    let key = format!("after-refusal-{}", doc.len_ops());
    doc.get_map("recovery").insert(&key, true).unwrap();
    doc.commit();
}

pub fn current_vv(doc: &LoroDoc) -> VersionVector {
    doc.oplog_vv()
}

use loro::{update_reuse_work, StoredUpdateReader, UpdateBlockCache, ID};

fn compare_exports(source: &LoroDoc, spans: &[IdSpan]) -> (Vec<u8>, Vec<u8>) {
    let control = source
        .export(ExportMode::updates_in_range(spans.to_vec()))
        .unwrap();
    let reused = source
        .export_updates_in_range_reusing_blocks(spans)
        .unwrap();
    let a = LoroDoc::new();
    let b = LoroDoc::new();
    let sa = a.import(&control).unwrap();
    let sb = b.import(&reused).unwrap();
    assert_eq!(format!("{sa:?}"), format!("{sb:?}"));
    assert_native_equal(&a, &b);
    let all = source.export(ExportMode::all_updates()).unwrap();
    assert!(a.import(&all).unwrap().pending.is_none());
    assert!(b.import(&all).unwrap().pending.is_none());
    assert_native_equal(&a, source);
    assert_native_equal(&b, source);
    (control, reused)
}

#[test]
fn e_cached_whole_and_partial_ranges_preserve_native_operations() {
    let original = map_history(41, 2600);
    let source = load(&snapshot(&original), 41);
    let full_span = IdSpan::new(41, 0, 2600);
    let blocks = source.update_source_blocks(&[full_span]);
    assert!(
        blocks.len() >= 4,
        "needs several independently encoded blocks: {blocks:?}"
    );
    assert!(blocks.iter().all(|b| b.cache != UpdateBlockCache::Changes));
    let middle = &blocks[1];
    for (label, span, reused_min, partial) in [
        (
            "single_partial",
            IdSpan::new(41, middle.start + 1, middle.end - 1),
            0,
            true,
        ),
        (
            "aligned",
            IdSpan::new(41, middle.start, blocks[2].end),
            2,
            false,
        ),
        (
            "partial_edges",
            IdSpan::new(41, blocks[0].start + 1, blocks[3].end - 1),
            2,
            true,
        ),
    ] {
        let fresh = load(&snapshot(&original), 41);
        update_reuse_work::start();
        let result = fresh
            .export_updates_in_range_reusing_blocks(&[span])
            .unwrap();
        let work = update_reuse_work::finish();
        assert!(work.reused_cached_blocks >= reused_min, "{label}: {work:?}");
        assert_eq!(work.newly_encoded_whole_blocks, 0);
        if partial {
            assert!(work.encoded_blocks > 0, "{label}: {work:?}");
        } else {
            assert_eq!(work.encoded_blocks, 0);
            assert_eq!(work.decoded_blocks, 0);
        }
        let (control, compared) = compare_exports(&fresh, &[span]);
        assert_eq!(result, compared);
        eprintln!(
            "coverage {label}: {work:?}; bytes_equal={}",
            control == result
        );
    }
    for spans in [
        vec![],
        vec![IdSpan::new(41, 9, 9)],
        vec![IdSpan::new(41, i32::MIN, 3)],
        vec![IdSpan::new(41, 11, 4)],
        vec![IdSpan::new(41, 1, 9), IdSpan::new(41, 4, 12)],
        vec![IdSpan::new(41, 1, 3), IdSpan::new(41, 20, 22)],
        vec![IdSpan::new(999, 0, 7)],
    ] {
        compare_exports(&source, &spans);
    }
}

#[test]
fn e_new_whole_is_not_cached_reuse_and_mutated_tail_is_not_stale() {
    let doc = map_history(51, 30);
    update_reuse_work::start();
    let first = doc
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    let initial = update_reuse_work::finish();
    assert!(initial.newly_encoded_whole_blocks > 0, "{initial:?}");
    assert_eq!(initial.reused_cached_blocks, 0);
    update_reuse_work::start();
    let repeated = doc
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    let cached = update_reuse_work::finish();
    assert_eq!(first, repeated);
    assert!(cached.reused_cached_blocks > 0);
    assert_eq!(cached.encoded_blocks, 0);
    let before = doc.oplog_vv();
    doc.get_map("history")
        .insert("tail", "mutated after caching")
        .unwrap();
    commit(&doc, 9999);
    let control = doc.export(ExportMode::updates(&before)).unwrap();
    let reused = doc.export_updates_reusing_blocks(&before).unwrap();
    let a = load(&first, 52);
    let b = load(&first, 53);
    a.import(&control).unwrap();
    b.import(&reused).unwrap();
    assert_native_equal(&a, &doc);
    assert_native_equal(&b, &doc);
    eprintln!("new_whole={initial:?}; repeated={cached:?}");
}

#[test]
fn r_native_queries_match_independent_oplog_oracle() {
    let doc = map_history(61, 2600);
    let encoded = doc.export(ExportMode::all_updates()).unwrap();
    update_reuse_work::start();
    let reader = StoredUpdateReader::new(encoded.clone()).unwrap();
    let built = update_reuse_work::finish();
    assert!(built.indexed_blocks >= 4);
    assert_eq!(built.decoded_blocks, 0);
    assert!(built.checksum_scanned_bytes >= encoded.len() as u64 - 20);
    for counter in [0, 1, 233, 799, 2599] {
        let expected =
            doc.with_oplog(|log| log.get_remote_change_at(ID::new(61, counter)).unwrap());
        assert_eq!(expected.id().counter, counter);
        assert_eq!(expected.len(), 1);
        update_reuse_work::start();
        let got = reader.query(IdSpan::new(61, counter, counter + 1)).unwrap();
        let query = update_reuse_work::finish();
        assert_eq!(got, vec![expected]);
        assert_eq!(query.decoded_blocks, 1);
        assert_eq!(query.selected_blocks, 1);
        assert!(query.decoded_bytes < encoded.len() as u64);
    }
    assert!(reader.query(IdSpan::new(61, 7, 7)).unwrap().is_empty());
    assert!(reader.query(IdSpan::new(999, 0, 100)).unwrap().is_empty());
    let mut got = reader.query_json(IdSpan::new(61, i32::MIN, 2)).unwrap();
    let mut expected = doc.export_json_in_id_span(IdSpan::new(61, 0, 2));
    assert_eq!(
        normalize_changes(&mut got),
        normalize_changes(&mut expected)
    );
    let mut reversed = reader.query_json(IdSpan::new(61, 5, 2)).unwrap();
    let mut reversed_span = IdSpan::new(61, 5, 2);
    reversed_span.normalize_();
    let mut expected = doc.export_json_in_id_span(reversed_span);
    assert_eq!(
        normalize_changes(&mut reversed),
        normalize_changes(&mut expected)
    );
    assert_eq!(reader.bytes(), encoded);
    eprintln!("R full checksum/index cost: {built:?}");
}

#[test]
fn e_r_unicode_slices_duplicates_and_missing_dependencies() {
    let doc = LoroDoc::new();
    doc.set_peer_id(71).unwrap();
    doc.set_change_merge_interval(0);
    doc.get_text("text")
        .insert(0, "A中😀e\u{301}hello")
        .unwrap();
    commit(&doc, 1);
    let split = doc.oplog_vv().get(&71).copied().unwrap();
    doc.get_text("text").delete(1, 3).unwrap();
    commit(&doc, 2);
    doc.get_text("text").insert(1, "并发").unwrap();
    commit(&doc, 3);
    let end = doc.oplog_vv().get(&71).copied().unwrap();
    let first = doc
        .export_updates_in_range_reusing_blocks(&[IdSpan::new(71, 0, split)])
        .unwrap();
    let second = doc
        .export_updates_in_range_reusing_blocks(&[IdSpan::new(71, split, end)])
        .unwrap();
    import_two_ranges_before_prerequisites(&first, &second, &doc);
    let bytes = doc
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    let reader = StoredUpdateReader::new(bytes).unwrap();
    for counter in 0..end {
        let mut got = reader
            .query_json(IdSpan::new(71, counter, counter + 1))
            .unwrap();
        let mut expected = doc.export_json_in_id_span(IdSpan::new(71, counter, counter + 1));
        assert_eq!(
            normalize_changes(&mut got),
            normalize_changes(&mut expected),
            "atom {counter}"
        );
    }
}

#[test]
#[ignore = "requires exact-baseline external reader; run scripts/check-update-reuse-independent.sh"]
fn offline_late_before_checkpoint_merge_and_deleted_anchor() {
    use loro::cursor::Side;
    let original = LoroDoc::new();
    original.set_peer_id(81).unwrap();
    original.get_text("text").insert(0, "abcdef中😀").unwrap();
    original.commit();
    let anchor = original.get_text("text").get_cursor(2, Side::Left).unwrap();
    let base = original.oplog_vv();
    let initial = snapshot(&original);
    let offline = load(&initial, 82);
    offline.get_text("text").insert(2, "late").unwrap();
    offline.commit();
    original.get_text("text").delete(1, 4).unwrap();
    original.commit();
    original.get_text("text").push_str("local").unwrap();
    original.commit();
    let checkpoint = snapshot(&original);
    let control_remote = offline.export(ExportMode::updates(&base)).unwrap();
    let reused_remote = offline.export_updates_reusing_blocks(&base).unwrap();
    let control = load(&checkpoint, 83);
    let reused = load(&checkpoint, 84);
    assert!(control.import(&control_remote).unwrap().pending.is_none());
    assert!(reused.import(&reused_remote).unwrap().pending.is_none());
    assert_native_equal(&control, &reused);
    assert_eq!(
        control.get_cursor_pos(&anchor).unwrap().current,
        reused.get_cursor_pos(&anchor).unwrap().current
    );
    let reverse = load(&snapshot(&offline), 85);
    let local = original.export_updates_reusing_blocks(&base).unwrap();
    reverse.import(&local).unwrap();
    assert_native_equal(&control, &reverse);
    let duplicate = reused.import(&reused_remote).unwrap();
    assert!(duplicate.success.is_empty());
    assert_native_equal(&reused, &control);
    unchanged_reader_agrees(
        &[&checkpoint, &control_remote],
        &[&checkpoint, &reused_remote],
    );
}

fn unchanged_reader_agrees(control: &[&[u8]], reused: &[&[u8]]) {
    let Ok(binary) = std::env::var("LORO_BASELINE_READER") else {
        panic!("set LORO_BASELINE_READER to the exact baseline independent_reader binary");
    };
    let dir = std::env::temp_dir().join(format!(
        "loro-independent-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |tag: &str, updates: &[&[u8]]| {
        let mut cmd = std::process::Command::new(&binary);
        for (n, bytes) in updates.iter().enumerate() {
            let path = dir.join(format!("{tag}-{n}.loro"));
            std::fs::write(&path, bytes).unwrap();
            cmd.arg(path);
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(run("control", control), run("reused", reused));
}

#[test]
#[ignore = "requires exact-baseline external reader; run scripts/check-update-reuse-independent.sh"]
fn e_generated_updates_are_read_by_exact_unchanged_binary() {
    let doc = map_history(91, 700);
    let cold = load(&snapshot(&doc), 91);
    let control = cold.export(ExportMode::all_updates()).unwrap();
    let reused = cold
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    unchanged_reader_agrees(&[&control], &[&reused]);
}

fn reseal(bytes: &mut [u8]) {
    let sum = xxhash_rust::xxh32::xxh32(&bytes[20..], u32::from_le_bytes(*b"LORO"));
    bytes[16..20].copy_from_slice(&sum.to_le_bytes());
}

#[test]
fn corruption_rejected_and_remote_import_stays_atomic() {
    let source = map_history(101, 1400);
    let valid = source
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    let reader = StoredUpdateReader::new(valid.clone()).unwrap();
    assert!(reader.blocks().len() > 2);
    let target = LoroDoc::new();
    target.set_peer_id(102).unwrap();
    let mut bad_checksum = valid.clone();
    *bad_checksum.last_mut().unwrap() ^= 0x80;
    assert!(StoredUpdateReader::new(bad_checksum.clone()).is_err());
    assert_invalid_import_atomic(&target, &bad_checksum);
    for length in [0, 1, 4, 20, 21, valid.len() - 1] {
        let truncated = valid[..length].to_vec();
        assert!(StoredUpdateReader::new(truncated.clone()).is_err());
        assert_invalid_import_atomic(&target, &truncated);
    }
    for body in [vec![0], vec![0xff; 11], vec![2, 1]] {
        let mut malformed = valid[..22].to_vec();
        malformed.extend(body);
        reseal(&mut malformed);
        assert!(StoredUpdateReader::new(malformed.clone()).is_err());
        assert_invalid_import_atomic(&target, &malformed);
    }
    let last = reader.blocks().last().unwrap();
    let mut malformed = valid.clone();
    malformed[last.payload_offset..last.payload_offset + last.payload_len].fill(0xff);
    reseal(&mut malformed);
    assert!(StoredUpdateReader::new(malformed.clone()).is_err());
    assert_invalid_import_atomic(&target, &malformed);
    // The public remote import path still performs full decoding, independent of R.
    assert!(target.import(&valid).unwrap().pending.is_none());
    assert_eq!(
        target.get_map("history").get_value(),
        source.get_map("history").get_value()
    );
}

fn read_leb(bytes: &[u8], offset: &mut usize) -> usize {
    let mut n = 0usize;
    for shift in (0..64).step_by(7) {
        let b = bytes[*offset];
        *offset += 1;
        n |= ((b & 127) as usize) << shift;
        if b & 128 == 0 {
            return n;
        }
    }
    panic!("invalid fixture varint")
}

#[test]
#[ignore = "requires exact-baseline external reader; run scripts/check-update-reuse-independent.sh"]
fn r_unselected_malformed_payload_is_not_remote_validation() {
    let doc = map_history(111, 1400);
    let valid = doc
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    let index = StoredUpdateReader::new(valid.clone()).unwrap();
    let first = index.blocks().first().unwrap().clone();
    let last = index.blocks().last().unwrap().clone();
    assert_ne!(first.start, last.start);
    // EncodedBlock is five u32 fields followed by header and change_meta byte slices.
    // Keep block framing + causal header valid while corrupting the unselected
    // change timestamps/messages. Construction may succeed on this trusted-only API.
    let mut corrupted = valid.clone();
    let mut at = last.payload_offset;
    for _ in 0..5 {
        read_leb(&corrupted, &mut at);
    }
    let header_len = read_leb(&corrupted, &mut at);
    at += header_len;
    let meta_len = read_leb(&corrupted, &mut at);
    assert!(meta_len > 8);
    corrupted[at..at + meta_len].fill(0xff);
    reseal(&mut corrupted);
    let selective = StoredUpdateReader::new(corrupted.clone())
        .expect("header-only index cannot attest payload validity");
    assert!(selective
        .query(IdSpan::new(111, first.start, first.start + 1))
        .is_ok());
    assert!(selective
        .query(IdSpan::new(111, last.start, last.start + 1))
        .is_err());
    let target = LoroDoc::new();
    target.set_peer_id(112).unwrap();
    assert_invalid_import_atomic(&target, &corrupted);
    target.import(&valid).unwrap();
    let conflicting = LoroDoc::new();
    conflicting.set_peer_id(111).unwrap();
    conflicting
        .get_map("history")
        .insert("changed", true)
        .unwrap();
    conflicting.commit();
    let conflict = conflicting
        .export_updates_reusing_blocks(&VersionVector::default())
        .unwrap();
    let before = target.get_deep_value();
    let status = target.import(&conflict).unwrap();
    assert!(
        status.success.is_empty(),
        "fully known input remains the baseline no-op"
    );
    assert_eq!(target.get_deep_value(), before);
    unchanged_reader_agrees(&[&valid], &[&valid, &conflict]);
    eprintln!("trust boundary: index + unrelated query succeeded, selected corrupt block and unchanged remote importer rejected");
}

#[test]
#[ignore = "requires exact-baseline external reader; run scripts/check-update-reuse-independent.sh"]
fn r_duplicate_blocks_are_explicitly_unsupported_without_changing_remote_import() {
    let source = map_history(121, 30);
    let valid = source.export(ExportMode::all_updates()).unwrap();
    let mut duplicate = valid.clone();
    duplicate.extend_from_slice(&valid[22..]);
    reseal(&mut duplicate);
    assert!(StoredUpdateReader::new(duplicate.clone()).is_err());
    let target = LoroDoc::new();
    target.import(&duplicate).unwrap();
    assert_native_equal(&target, &source);
    unchanged_reader_agrees(&[&valid], &[&duplicate]);
    eprintln!("valid duplicate-block Update accepted by unchanged importer, rejected by opt-in R constructor");
}
