//! Differential evidence for the opt-in, restricted disjoint-import experiment.
//! OFF executes the unchanged native Loro path; equal strings alone are not an oracle.
#![cfg(feature = "strider-experiment")]

use loro::{
    cursor::{Cursor, Side},
    ExportMode, Frontiers, LoroDoc, VersionVector,
};
use loro_internal::diff_calc::strider_experiment::{
    reset_stats, set_borrowed_base, set_enabled, stats,
};
use loro_internal::event::EventTriggerKind;
use std::sync::{Arc, Mutex};

struct Enabled(bool);
impl Enabled {
    fn new(value: bool) -> Self {
        Self(set_enabled(value))
    }
}
impl Drop for Enabled {
    fn drop(&mut self) {
        set_enabled(self.0);
    }
}

#[derive(Clone, Copy, Debug)]
enum Edit {
    Insert(usize, &'static str),
    Delete(usize, usize),
    ReverseDelete(usize),
    Mark,
    OtherRoot,
    MultipleRuns,
}
fn edit(doc: &LoroDoc, op: Edit) {
    let text = doc.get_text("t");
    match op {
        Edit::Insert(pos, value) => text.insert(pos, value).unwrap(),
        Edit::Delete(pos, len) => text.delete(pos, len).unwrap(),
        Edit::ReverseDelete(pos) => {
            text.delete(pos + 1, 1).unwrap();
            text.delete(pos, 1).unwrap();
        }
        Edit::Mark => text.mark(1..3, "bold", true).unwrap(),
        Edit::OtherRoot => doc.get_text("other").insert(0, "other").unwrap(),
        Edit::MultipleRuns => {
            text.insert(1, "x").unwrap();
            text.insert(6, "y").unwrap();
        }
    }
    doc.commit();
}
fn load(bytes: &[u8], peer: u64) -> LoroDoc {
    let _off = Enabled::new(false);
    let doc = LoroDoc::new();
    doc.set_peer_id(peer).unwrap();
    doc.import(bytes).unwrap();
    doc
}
struct Inputs {
    before: Vec<u8>,
    remote: Vec<u8>,
    base_frontiers: Frontiers,
    local_frontiers: Frontiers,
    cursors: Vec<Cursor>,
}
fn inputs(body: &str, local: Edit, remote: Edit, marked: bool, shallow: bool) -> Inputs {
    inputs_with_message(body, local, remote, marked, shallow, false)
}
fn inputs_with_message(
    body: &str,
    local: Edit,
    remote: Edit,
    marked: bool,
    shallow: bool,
    message: bool,
) -> Inputs {
    let _off = Enabled::new(false);
    let base = LoroDoc::new();
    base.set_peer_id(1).unwrap();
    if message {
        base.set_next_commit_message("tag");
    }
    base.get_text("t").insert(0, body).unwrap();
    base.commit();
    if marked {
        edit(&base, Edit::Mark);
    }
    let base_frontiers = base.state_frontiers();
    let text = base.get_text("t");
    let n = text.len_unicode();
    let mut cursors = Vec::new();
    for pos in [0, 1, 2, n / 2, n - 2, n - 1, n] {
        for side in [Side::Left, Side::Middle, Side::Right] {
            if let Some(cursor) = text.get_cursor(pos, side) {
                cursors.push(cursor);
            }
        }
    }
    let bytes = base
        .export(if shallow {
            ExportMode::shallow_snapshot(&base_frontiers)
        } else {
            ExportMode::Snapshot
        })
        .unwrap();
    let left = load(&bytes, 2);
    let right = load(&bytes, 3);
    edit(&left, local);
    edit(&right, remote);
    Inputs {
        before: left.export(ExportMode::Snapshot).unwrap(),
        remote: right.export(ExportMode::updates(&base.oplog_vv())).unwrap(),
        base_frontiers,
        local_frontiers: left.state_frontiers(),
        cursors,
    }
}
fn equivalent(a: &LoroDoc, b: &LoroDoc, cursors: &[Cursor]) {
    assert_eq!(a.get_deep_value(), b.get_deep_value());
    assert_eq!(a.get_text("t").to_delta(), b.get_text("t").to_delta());
    assert_eq!(a.state_vv(), b.state_vv());
    assert_eq!(a.oplog_vv(), b.oplog_vv());
    assert_eq!(a.state_frontiers(), b.state_frontiers());
    assert_eq!(a.oplog_frontiers(), b.oplog_frontiers());
    assert_eq!(a.is_detached(), b.is_detached());
    // Original operation identities, dependencies, content and ordering—not a newly
    // synthesized text-only model. No peer-ID compression hides native identities.
    let ja =
        a.export_json_updates_without_peer_compression(&VersionVector::default(), &a.oplog_vv());
    let jb =
        b.export_json_updates_without_peer_compression(&VersionVector::default(), &b.oplog_vv());
    assert_eq!(
        serde_json::to_value(ja).unwrap(),
        serde_json::to_value(jb).unwrap()
    );
    for cursor in cursors {
        assert_eq!(
            a.get_cursor_pos(cursor).unwrap(),
            b.get_cursor_pos(cursor).unwrap()
        );
    }
    let ta = a.get_text("t");
    let tb = b.get_text("t");
    assert_eq!(ta.len_utf8(), tb.len_utf8());
    assert_eq!(ta.len_utf16(), tb.len_utf16());
    let n = ta.len_unicode();
    assert_eq!(n, tb.len_unicode());
    for pos in [0, n / 2, n.saturating_sub(1), n] {
        for side in [Side::Left, Side::Middle, Side::Right] {
            assert_eq!(ta.get_cursor(pos, side), tb.get_cursor(pos, side));
        }
    }
}
type EventRecord = (
    EventTriggerKind,
    String,
    Vec<(loro::ContainerID, Vec<loro::TextDelta>)>,
);
fn events(doc: &LoroDoc) -> (Arc<Mutex<Vec<EventRecord>>>, loro::Subscription) {
    let records = Arc::new(Mutex::new(Vec::new()));
    let capture = records.clone();
    let guard = doc.subscribe_root(Arc::new(move |event| {
        let deltas = event
            .events
            .iter()
            .map(|item| (item.target.clone(), item.diff.as_text().unwrap().clone()))
            .collect();
        capture
            .lock()
            .unwrap()
            .push((event.triggered_by, event.origin.to_owned(), deltas));
    }));
    (records, guard)
}
fn warm_history(doc: &LoroDoc) {
    // Explicit test setup, not a free property of snapshot reopen.
    drop(
        doc.export_json_updates_without_peer_compression(
            &VersionVector::default(),
            &doc.oplog_vv(),
        ),
    );
}
fn exercise_history(input: Inputs, expect_hit: bool, warm: bool, borrowed: bool) {
    struct Borrowed(bool);
    impl Drop for Borrowed {
        fn drop(&mut self) {
            set_borrowed_base(self.0);
        }
    }
    let _borrowed = Borrowed(set_borrowed_base(borrowed));
    let accelerated = load(&input.before, 9);
    let original = load(&input.before, 9);
    if warm {
        warm_history(&accelerated);
        warm_history(&original);
    }
    let (a_events, a_subscription) = events(&accelerated);
    let (b_events, b_subscription) = events(&original);
    {
        let _on = Enabled::new(true);
        reset_stats();
        accelerated.import(&input.remote).unwrap();
        let observed = stats();
        if expect_hit {
            assert_eq!(observed.hits, 1);
            if borrowed && !warm {
                assert_eq!(observed.borrowed_base_attempts, 1);
                assert_eq!(observed.borrowed_base_proofs, 1);
            } else {
                assert_eq!(observed.borrowed_base_proofs, 0);
            }
            assert!(
                observed.hits > 0,
                "eligible fixture must exercise the candidate: {observed:?}"
            );
        } else {
            assert_eq!(
                observed.hits, 0,
                "ineligible fixture must retain native replay"
            );
            if !warm && !borrowed {
                assert!(
                    observed.unparsed_fallbacks > 0,
                    "cold valid shape must report unparsed eligibility fallback"
                );
            }
        }
    }
    {
        let _off = Enabled::new(false);
        original.import(&input.remote).unwrap();
    }
    assert_eq!(*a_events.lock().unwrap(), *b_events.lock().unwrap());
    drop((a_subscription, b_subscription));
    equivalent(&accelerated, &original, &input.cursors);
    // Checkout retreat/advance and restore must retain the native history, not
    // just the visible endpoint produced by the experimental delta.
    reset_stats();
    for checkpoint in [&input.base_frontiers, &input.local_frontiers] {
        accelerated.checkout(checkpoint).unwrap();
        original.checkout(checkpoint).unwrap();
        equivalent(&accelerated, &original, &input.cursors);
        accelerated.checkout_to_latest();
        original.checkout_to_latest();
    }
    assert_eq!(
        stats().hits,
        0,
        "ordinary checkout must not mint the import capability"
    );
    for doc in [&accelerated, &original] {
        edit(doc, Edit::Mark);
        edit(doc, Edit::Insert(2, "🦀"));
        edit(doc, Edit::Delete(0, 1));
    }
    equivalent(&accelerated, &original, &input.cursors);
    let a_reload = load(&accelerated.export(ExportMode::Snapshot).unwrap(), 11);
    let b_reload = load(&original.export(ExportMode::Snapshot).unwrap(), 11);
    equivalent(&a_reload, &b_reload, &input.cursors);
    let future = load(&original.export(ExportMode::Snapshot).unwrap(), 12);
    edit(&future, Edit::Insert(0, "future"));
    let updates = future
        .export(ExportMode::updates(&original.oplog_vv()))
        .unwrap();
    for doc in [&a_reload, &b_reload] {
        edit(doc, Edit::Insert(0, "same-gap"));
        doc.import(&updates).unwrap();
    }
    equivalent(&a_reload, &b_reload, &input.cursors);
}

#[test]
fn cold_and_warm_three_paths_preserve_native_future_behavior() {
    for body in ["0123456789abcdefghijklmnop", "a😀é漢🦀bcdefghijklmnopqrst"] {
        let right = body.chars().count() - 3;
        for left in [Edit::Insert(1, "λ😀"), Edit::Delete(1, 2)] {
            for far in [Edit::Insert(right, "漢z"), Edit::Delete(right, 2)] {
                for warm in [false, true] {
                    for borrowed in [false, true] {
                        exercise_history(
                            inputs(body, left, far, false, false),
                            warm || borrowed,
                            warm,
                            borrowed,
                        );
                        exercise_history(
                            inputs(body, far, left, false, false),
                            warm || borrowed,
                            warm,
                            borrowed,
                        );
                    }
                }
            }
        }
    }
}
#[test]
fn borrowed_overlap_and_unsupported_shapes_keep_native_path() {
    let body = "0123456789abcdefghijklmnop";
    for warm in [false, true] {
        for (left, right) in [
            (Edit::Insert(2, "a"), Edit::Insert(2, "b")),
            (Edit::Delete(2, 3), Edit::Insert(5, "b")),
            (Edit::Delete(2, 3), Edit::Delete(5, 2)),
            (Edit::Insert(1, "a"), Edit::OtherRoot),
            (Edit::Insert(1, "a"), Edit::MultipleRuns),
            (Edit::Insert(1, "a"), Edit::Mark),
        ] {
            exercise_history(inputs(body, left, right, false, false), false, warm, true);
        }
        exercise_history(
            inputs(body, Edit::Insert(1, "a"), Edit::Delete(18, 2), true, false),
            false,
            warm,
            true,
        );
        exercise_history(
            inputs(body, Edit::Insert(1, "a"), Edit::Delete(18, 2), false, true),
            false,
            warm,
            true,
        );
    }
}
#[test]
fn borrowed_reverse_delete_preserves_native_ids() {
    for warm in [false, true] {
        exercise_history(
            inputs(
                "0123456789abcdefghijklmnop",
                Edit::ReverseDelete(1),
                Edit::Insert(20, "😀"),
                false,
                false,
            ),
            true,
            warm,
            true,
        );
    }
}

#[test]
fn cold_nonempty_commit_message_uses_native_fallback() {
    exercise_history(
        inputs_with_message(
            "0123456789abcdefghijklmnop",
            Edit::Insert(1, "a"),
            Edit::Delete(20, 2),
            false,
            false,
            true,
        ),
        false,
        false,
        true,
    );
}

struct BorrowedStrategy(bool);
impl Drop for BorrowedStrategy {
    fn drop(&mut self) {
        set_borrowed_base(self.0);
    }
}
#[test]
fn unlocking_pending_successor_preserves_original_path_and_future_state() {
    let _borrowed = BorrowedStrategy(set_borrowed_base(true));
    let _off = Enabled::new(false);
    let base = LoroDoc::new();
    base.set_peer_id(1).unwrap();
    base.get_text("t")
        .insert(0, "0123456789abcdefghijklmnop")
        .unwrap();
    base.commit();
    let snapshot = base.export(ExportMode::Snapshot).unwrap();
    let left = load(&snapshot, 2);
    let remote = load(&snapshot, 3);
    edit(&left, Edit::Insert(1, "local"));
    edit(&remote, Edit::Insert(20, "first"));
    let first_vv = remote.oplog_vv();
    let first = remote
        .export(ExportMode::updates(&base.oplog_vv()))
        .unwrap();
    edit(&remote, Edit::Insert(22, "second"));
    let successor = remote.export(ExportMode::updates(&first_vv)).unwrap();
    let before = left.export(ExportMode::Snapshot).unwrap();
    let a = load(&before, 9);
    let b = load(&before, 9);
    let initial_state = a.get_deep_value();
    for update in [&successor, &first] {
        {
            let _on = Enabled::new(true);
            reset_stats();
            a.import(update).unwrap();
            assert_eq!(stats().hits, 0);
        }
        b.import(update).unwrap();
        equivalent(&a, &b, &[]);
        if std::ptr::eq(update, &successor) {
            assert_eq!(a.get_deep_value(), initial_state);
        }
    }
    edit(&a, Edit::Insert(0, "future"));
    edit(&b, Edit::Insert(0, "future"));
    equivalent(&a, &b, &[]);
}

#[test]
fn valid_wire_prefix_then_suffix_preserves_complete_native_history() {
    let _borrowed = BorrowedStrategy(set_borrowed_base(true));
    let _off = Enabled::new(false);
    let base = LoroDoc::new();
    base.set_peer_id(1).unwrap();
    base.get_text("t")
        .insert(0, "0123456789abcdefghijklmnop")
        .unwrap();
    base.commit();
    let snapshot = base.export(ExportMode::Snapshot).unwrap();
    let left = load(&snapshot, 2);
    let remote = load(&snapshot, 3);
    edit(&left, Edit::Insert(1, "local"));
    edit(&remote, Edit::Insert(20, "abcdef"));
    let prefix = remote
        .export(ExportMode::updates_in_range(vec![loro::IdSpan::new(
            3, 0, 3,
        )]))
        .unwrap();
    let suffix = remote
        .export(ExportMode::updates_in_range(vec![loro::IdSpan::new(
            3, 3, 6,
        )]))
        .unwrap();
    let before = left.export(ExportMode::Snapshot).unwrap();
    let a = load(&before, 9);
    let b = load(&before, 9);
    for (ordinal, update) in [&prefix, &suffix].into_iter().enumerate() {
        {
            let _on = Enabled::new(true);
            reset_stats();
            a.import(update).unwrap();
            if ordinal == 1 {
                assert_eq!(
                    stats().hits,
                    0,
                    "existing peer suffix is outside the three-whole-run shape"
                );
            }
        }
        b.import(update).unwrap();
        equivalent(&a, &b, &[]);
    }
    // A full original-route import must expose exactly the same native history.
    let fresh = load(&before, 9);
    fresh
        .import(&remote.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    // Different transport chunking can retain abc+def versus abcdef operation
    // runs even in native Loro. Exact JSON remains required for the identical
    // ON/OFF transport above; here compare materialized native atom identities.
    for round in 0..2 {
        assert_eq!(a.get_deep_value(), fresh.get_deep_value());
        assert_eq!(a.state_vv(), fresh.state_vv());
        assert_eq!(a.oplog_vv(), fresh.oplog_vv());
        assert_eq!(a.state_frontiers(), fresh.state_frontiers());
        assert_eq!(a.get_text("t").to_delta(), fresh.get_text("t").to_delta());
        for pos in 0..=a.get_text("t").len_unicode() {
            for side in [Side::Left, Side::Middle, Side::Right] {
                assert_eq!(
                    a.get_text("t").get_cursor(pos, side),
                    fresh.get_text("t").get_cursor(pos, side)
                );
            }
        }
        if round == 0 {
            for doc in [&a, &b, &fresh] {
                edit(doc, Edit::Insert(0, "future"));
            }
            equivalent(&a, &b, &[]);
        }
    }
}

#[test]
#[ignore = "bounded same-binary fresh-process timing; explicit runner only"]
fn strider_borrowed_import_timing() {
    use std::time::Instant;
    let _borrowed = BorrowedStrategy(set_borrowed_base(true));
    let size: usize = std::env::var("STRIDER_SIZE").unwrap().parse().unwrap();
    assert!([32768, 262144].contains(&size));
    let enabled = std::env::var("STRIDER_ENABLED").unwrap() == "1";
    let output = std::env::var("STRIDER_OUTPUT").unwrap();
    let body = "a".repeat(size);
    let mut fixtures = Vec::new();
    for left in [Edit::Insert(8, "local"), Edit::Delete(8, 3)] {
        for right in [Edit::Insert(size - 8, "remote"), Edit::Delete(size - 8, 3)] {
            fixtures.push(inputs(&body, left, right, false, false));
        }
    }
    let mut samples = Vec::with_capacity(20);
    for iteration in 0..24 {
        let input = &fixtures[iteration % fixtures.len()];
        let measured = load(&input.before, 9);
        let original = load(&input.before, 9);
        let previous = Enabled::new(enabled);
        reset_stats();
        let start = Instant::now();
        let result = measured.import(&input.remote);
        let elapsed = start.elapsed().as_nanos();
        let observed = stats();
        drop(previous);
        result.unwrap();
        assert_eq!(observed.hits, u64::from(enabled));
        assert_eq!(observed.borrowed_base_proofs, u64::from(enabled));
        {
            let _off = Enabled::new(false);
            original.import(&input.remote).unwrap();
        }
        equivalent(&measured, &original, &input.cursors);
        // Same native author/next op after import; assertions and native history
        // export stay outside the timed interval.
        edit(&measured, Edit::Insert(0, "next"));
        edit(&original, Edit::Insert(0, "next"));
        equivalent(&measured, &original, &input.cursors);
        if iteration >= 4 {
            samples.push(serde_json::json!({"shape":iteration % 4,"import_ns":elapsed,"attempts":observed.attempts,"hits":observed.hits,"borrowed_base_attempts":observed.borrowed_base_attempts,"borrowed_base_proofs":observed.borrowed_base_proofs}));
        }
    }
    // Untimed explicit fallback control in the very same binary/process.
    exercise_history(
        inputs(
            "0123456789abcdefghijklmnop",
            Edit::Insert(2, "a"),
            Edit::Insert(2, "b"),
            false,
            false,
        ),
        false,
        false,
        true,
    );
    std::fs::write(output, serde_json::to_vec_pretty(&serde_json::json!({
        "schema":1,"size":size,"enabled":enabled,"warmup":4,"samples":samples,
        "notes":"Snapshot-loaded history, no prewarm. Borrowed encoded metadata proof enabled for candidate, faithful native OFF control. Same feature binary, import-only wall time, inputs/session loads outside timing; native identity/future-edit and fallback oracle afterwards. Not a general CRDT engine or end-user latency benchmark."
    })).unwrap()).unwrap();
}
