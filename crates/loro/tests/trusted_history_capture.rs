#![cfg(feature = "trusted_history_experiment")]
use loro::{
    guard_trusted_history, HistoryEntry, HistoryValue, LoroDoc, TrustedHistoryCapture,
    TrustedHistorySource,
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
#[derive(Debug)]
struct Source {
    values: BTreeMap<Vec<u8>, Vec<u8>>,
    reads: AtomicUsize,
    fail: AtomicBool,
}
impl TrustedHistorySource for Source {
    fn read(&self, key: &[u8]) -> Result<Vec<u8>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err("injected immutable backing read failure".into());
        }
        self.values
            .get(key)
            .cloned()
            .ok_or("missing committed history object".into())
    }
}
fn persist(
    cap: TrustedHistoryCapture,
    old: Option<&Source>,
) -> (Vec<u8>, Vec<HistoryEntry>, Arc<Source>) {
    let mut values = BTreeMap::new();
    for (key, value) in cap.history {
        let bytes = match value {
            HistoryValue::Written(bytes) => bytes,
            HistoryValue::Retained { bytes_len } => {
                let bytes = old.unwrap().values[&key].clone();
                assert_eq!(bytes.len(), bytes_len);
                bytes
            }
        };
        values.insert(key, bytes);
    }
    let entries = values
        .iter()
        .map(|(key, bytes)| HistoryEntry {
            key: key.clone(),
            bytes_len: bytes.len(),
        })
        .collect();
    (
        cap.state,
        entries,
        Arc::new(Source {
            values,
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }),
    )
}
#[test]
fn trusted_history_cow_capture_reopen_retains_unread_old_bodies() {
    let original = LoroDoc::new();
    original.set_peer_id(1).unwrap();
    original.get_text("text").insert(0, "abcdef").unwrap();
    original.commit();
    let old_device = original.fork_cow().unwrap();
    old_device.set_peer_id(9100).unwrap();
    let old_base = old_device.oplog_vv();
    old_device.get_text("text").insert(2, "old").unwrap();
    old_device.commit();
    let old_update = old_device
        .export(loro::ExportMode::updates(&old_base))
        .unwrap();
    for i in 0..32 {
        original.set_peer_id(2 + i % 4).unwrap();
        let s = "old-history".repeat(128);
        original.get_text("text").insert(6, &s).unwrap();
        original.commit();
        original.get_text("text").delete(6, s.len()).unwrap();
        original.commit();
    }
    let capture = original.capture_trusted_history().unwrap();
    assert!(capture.bootstrap_full_export);
    let (state, entries, source) = persist(capture, None);
    let opened = LoroDoc::open_trusted_history(&state, entries.clone(), source.clone()).unwrap();
    assert!(!opened.is_shallow());
    let accepted_vv = opened.oplog_vv();
    let candidate = guard_trusted_history(|| {
        let c = opened.fork_cow().unwrap();
        c.set_peer_id(9000).unwrap();
        c.get_text("text").insert(0, "new").unwrap();
        c.commit();
        c
    })
    .unwrap();
    let before = source.reads.load(Ordering::SeqCst);
    let next = candidate.capture_trusted_history().unwrap();
    assert!(!next.bootstrap_full_export);
    assert_eq!(
        source.reads.load(Ordering::SeqCst),
        before,
        "capture must not fetch untouched historical payloads"
    );
    let retained = next
        .history
        .iter()
        .filter(|(_, v)| matches!(v, HistoryValue::Retained { .. }))
        .count();
    assert!(retained > 8, "must retain real old blocks");
    let again = candidate.capture_trusted_history().unwrap();
    assert_eq!(
        next.history
            .iter()
            .filter(|(_, v)| matches!(v, HistoryValue::Written(_)))
            .count(),
        again
            .history
            .iter()
            .filter(|(_, v)| matches!(v, HistoryValue::Written(_)))
            .count(),
        "capture alone must not rebase an unpersisted overlay"
    );
    let (state2, entries2, source2) = persist(next, Some(&source));
    let reopened = LoroDoc::open_trusted_history(&state2, entries2, source2).unwrap();
    assert_eq!(candidate.get_deep_value(), reopened.get_deep_value());
    assert_eq!(candidate.oplog_vv(), reopened.oplog_vv());
    assert_eq!(opened.get_text("text").to_string(), "abcdef");
    candidate.get_text("text").insert(0, "second").unwrap();
    candidate.commit();
    let before_second = source.reads.load(Ordering::SeqCst);
    let second = candidate.capture_trusted_history().unwrap();
    assert!(!second.bootstrap_full_export);
    assert_eq!(source.reads.load(Ordering::SeqCst), before_second);
    // Persist against the ORIGINAL source, not the first captured overlay.
    let (state3, entries3, source3) = persist(second, Some(&source));
    let reopened_second = LoroDoc::open_trusted_history(&state3, entries3, source3).unwrap();
    assert_eq!(candidate.get_deep_value(), reopened_second.get_deep_value());
    assert_eq!(candidate.oplog_vv(), reopened_second.oplog_vv());
    guard_trusted_history(|| candidate.import(&old_update))
        .unwrap()
        .unwrap();
    let merged = candidate.capture_trusted_history().unwrap();
    assert!(!merged.bootstrap_full_export);
    assert!(
        merged
            .history
            .iter()
            .filter(|(_, v)| matches!(v, HistoryValue::Retained { .. }))
            .count()
            > 8
    );
    let (merged_state, merged_entries, merged_source) = persist(merged, Some(&source));
    let merged_reopen =
        LoroDoc::open_trusted_history(&merged_state, merged_entries, merged_source).unwrap();
    assert_eq!(candidate.get_deep_value(), merged_reopen.get_deep_value());
    assert_eq!(candidate.oplog_vv(), merged_reopen.oplog_vv());
    // The unavailable source is never mapped to absent operations. Only this
    // isolated branch is discarded; capture after failure cannot mint a root.
    let failed = opened.fork_cow().unwrap();
    source.fail.store(true, Ordering::SeqCst);
    let result = guard_trusted_history(|| failed.export(loro::ExportMode::Snapshot));
    assert!(result.is_err());
    assert!(failed.capture_trusted_history().is_err());
    assert!(failed.fork_cow().is_err());
    drop(failed); // real disposal, never mem::forget or clearing lock poison
    source.fail.store(false, Ordering::SeqCst);
    let failed_import = opened.fork_cow().unwrap();
    source.fail.store(true, Ordering::SeqCst);
    assert!(guard_trusted_history(|| failed_import.import(&old_update)).is_err());
    assert!(failed_import.capture_trusted_history().is_err());
    drop(failed_import);
    source.fail.store(false, Ordering::SeqCst);
    assert_eq!(opened.get_text("text").to_string(), "abcdef");
    assert!(opened.fork_cow().is_ok());
    assert_eq!(opened.oplog_vv(), accepted_vv);
    let retry = LoroDoc::open_trusted_history(&state, entries.clone(), source.clone()).unwrap();
    retry.get_text("text").insert(0, "retry").unwrap();
    retry.commit();
    assert_eq!(retry.get_text("text").to_string(), "retryabcdef");
    assert!(
        !retry
            .capture_trusted_history()
            .unwrap()
            .bootstrap_full_export
    );
    assert!(LoroDoc::open_trusted_history(
        &state,
        vec![entries[0].clone(), entries[0].clone()],
        source
    )
    .is_err());
}
#[test]
fn trusted_history_guard_does_not_swallow_unrelated_panics() {
    let result =
        std::panic::catch_unwind(|| guard_trusted_history(|| panic!("unrelated invariant")));
    assert!(result.is_err());
}

// Newly reconstructed regression tests after workspace loss. These are not a
// byte-for-byte restoration of the previous unpublished tests or their evidence.
#[test]
fn trusted_history_latest_state_vv_does_not_read_old_blocks() {
    let author = LoroDoc::new();
    author.set_peer_id(1).unwrap();
    author.get_text("text").insert(0, "abcdef").unwrap();
    author.commit();
    for i in 0..32 {
        author.set_peer_id(2 + i % 4).unwrap();
        let payload = "old-history".repeat(128);
        author.get_text("text").insert(6, &payload).unwrap();
        author.commit();
        author.get_text("text").delete(6, payload.len()).unwrap();
        author.commit();
    }
    let (state, entries, source) = persist(author.capture_trusted_history().unwrap(), None);
    let opened = LoroDoc::open_trusted_history(&state, entries, source.clone()).unwrap();
    assert_eq!(opened.state_frontiers(), opened.oplog_frontiers());
    let before = source.reads.load(Ordering::SeqCst);
    let actual = opened.state_vv();
    let after = source.reads.load(Ordering::SeqCst);
    eprintln!("STATE_VV_REDO latest before={before} after={after} delta={}", after - before);
    assert_eq!(actual, opened.oplog_vv());
    assert_eq!(after, before, "latest-frontier state_vv must not fetch old history bodies");

    let branch = opened.fork_cow().unwrap();
    let before_cow = source.reads.load(Ordering::SeqCst);
    let branch_vv = branch.state_vv();
    let after_cow = source.reads.load(Ordering::SeqCst);
    eprintln!("STATE_VV_REDO cow before={before_cow} after={after_cow}");
    assert_eq!(after_cow, before_cow);
    assert_eq!(branch_vv, actual);
    // The original DAG oracle runs AFTER the read-count checks, never warming
    // this source before the property under test has been measured.
    assert_eq!(Some(actual), opened.frontiers_to_vv(&opened.state_frontiers()));
    assert_eq!(Some(branch_vv), branch.frontiers_to_vv(&branch.state_frontiers()));
}

#[test]
fn state_vv_latest_shortcut_matches_original_frontier_algorithm() {
    fn check(doc: &LoroDoc, label: &str) {
        // Capture the tested result first: frontiers_to_vv is the original DAG
        // path, not a second invocation of the optimized API.
        let actual = doc.state_vv();
        let oracle = doc.frontiers_to_vv(&doc.state_frontiers()).unwrap();
        assert_eq!(actual, oracle, "{label}");
        eprintln!("STATE_VV_REDO oracle {label}");
    }
    let doc = LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    check(&doc, "empty");
    doc.get_text("text").insert(0, "abcdef").unwrap();
    check(&doc, "uncommitted initial insertion");
    doc.commit();
    check(&doc, "latest committed");
    let historical = doc.state_frontiers();
    let old_device = doc.fork_cow().unwrap();
    old_device.set_peer_id(99).unwrap();
    let old_base = old_device.oplog_vv();
    old_device.get_text("text").insert(2, "OLD").unwrap();
    old_device.commit();
    let old_update = old_device.export(loro::ExportMode::updates(&old_base)).unwrap();

    let branch = doc.fork_cow().unwrap();
    branch.set_peer_id(2).unwrap();
    check(&branch, "CoW current");
    branch.get_text("text").insert(0, "branch").unwrap();
    check(&branch, "CoW uncommitted");
    branch.commit();
    check(&branch, "CoW committed");
    doc.get_text("text").delete(1, 1).unwrap();
    check(&doc, "uncommitted deletion");
    doc.commit();
    doc.checkout(&historical).unwrap();
    assert!(doc.is_detached());
    check(&doc, "detached historical");
    doc.import(&old_update).unwrap();
    assert!(doc.is_detached());
    check(&doc, "detached after old-device import");
    doc.attach();
    check(&doc, "reattached old-device merge");
    assert!(doc.get_text("text").to_string().contains("OLD"));

    let full = LoroDoc::new();
    full.set_peer_id(10).unwrap();
    full.get_text("text").insert(0, "abcdef").unwrap();
    full.commit();
    let cut = full.state_frontiers();
    full.get_text("text").insert(6, "tail").unwrap();
    full.commit();
    let bytes = full.export(loro::ExportMode::shallow_snapshot(&cut)).unwrap();
    let shallow = LoroDoc::new();
    shallow.import(&bytes).unwrap();
    assert!(shallow.is_shallow());
    check(&shallow, "shallow latest");
    shallow.checkout(&cut).unwrap();
    check(&shallow, "shallow retained historical");
    shallow.attach();
    shallow.set_peer_id(11).unwrap();
    shallow.get_text("text").insert(0, "new").unwrap();
    check(&shallow, "shallow uncommitted");
    shallow.commit();
    check(&shallow, "shallow edited latest");
}
