//! Supplemental process-isolated metrics; original experiment/production unchanged.
#![cfg(feature = "strider-experiment")]

use loro::{cursor::Side, ExportMode, LoroDoc, VersionVector};
use loro_internal::diff_calc::strider_experiment::{
    reset_stats, set_borrowed_base, set_enabled, stats,
};
use serde_json::json;
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

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

fn load(bytes: &[u8], peer: u64) -> LoroDoc {
    let _off = Enabled::new(false);
    let doc = LoroDoc::new();
    doc.set_peer_id(peer).unwrap();
    doc.import(bytes).unwrap();
    doc
}
fn warm_history(doc: &LoroDoc) -> u128 {
    let start = Instant::now();
    // The explicit history export is dropped before sampling: keep decoded
    // history, not the temporary JSON result. This work is never free.
    drop(
        doc.export_json_updates_without_peer_compression(
            &VersionVector::default(),
            &doc.oplog_vv(),
        ),
    );
    start.elapsed().as_nanos()
}
fn fixture(size: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let _off = Enabled::new(false);
    let base = LoroDoc::new();
    base.set_peer_id(1).unwrap();
    base.get_text("t").insert(0, &"a".repeat(size)).unwrap();
    base.commit();
    let snapshot = base.export(ExportMode::Snapshot).unwrap();
    let local = load(&snapshot, 2);
    let remote = load(&snapshot, 3);
    local.get_text("t").insert(8, "local").unwrap();
    local.commit();
    remote.get_text("t").insert(size - 8, "remote").unwrap();
    remote.commit();
    let before = local.export(ExportMode::Snapshot).unwrap();
    let update = remote
        .export(ExportMode::updates(&base.oplog_vv()))
        .unwrap();
    local.import(&update).unwrap();
    let merged = local.export(ExportMode::Snapshot).unwrap();
    (before, update, merged)
}
fn equivalent(a: &LoroDoc, b: &LoroDoc) {
    assert_eq!(a.get_deep_value(), b.get_deep_value());
    assert_eq!(a.get_text("t").to_delta(), b.get_text("t").to_delta());
    assert_eq!(a.state_vv(), b.state_vv());
    assert_eq!(a.oplog_vv(), b.oplog_vv());
    assert_eq!(a.state_frontiers(), b.state_frontiers());
    assert_eq!(a.oplog_frontiers(), b.oplog_frontiers());
    assert_eq!(a.is_detached(), b.is_detached());
    let x =
        a.export_json_updates_without_peer_compression(&VersionVector::default(), &a.oplog_vv());
    let y =
        b.export_json_updates_without_peer_compression(&VersionVector::default(), &b.oplog_vv());
    // Independent snapshot import may enumerate concurrent changes differently.
    // Sort only the outer change set, preserving every native field and op order.
    let canonical_history = |mut history: serde_json::Value| {
        let changes = history["changes"].as_array_mut().unwrap();
        changes.sort_by_key(|change| change["id"].as_str().unwrap().to_owned());
        assert!(changes
            .windows(2)
            .all(|pair| pair[0]["id"] != pair[1]["id"]));
        history
    };
    assert_eq!(
        canonical_history(serde_json::to_value(x).unwrap()),
        canonical_history(serde_json::to_value(y).unwrap())
    );
    let n = a.get_text("t").len_unicode();
    for pos in [0, 7, 8, 9, n / 2, n - 9, n - 8, n] {
        for side in [Side::Left, Side::Middle, Side::Right] {
            let ca = a.get_text("t").get_cursor(pos, side);
            let cb = b.get_text("t").get_cursor(pos, side);
            assert_eq!(ca, cb);
            if let Some(cursor) = ca {
                assert_eq!(
                    a.get_cursor_pos(&cursor).unwrap(),
                    b.get_cursor_pos(&cursor).unwrap()
                );
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Memory {
    rss_kib: u64,
    hwm_kib: u64,
}
impl Memory {
    fn json(&self) -> serde_json::Value {
        json!({"rss_kib":self.rss_kib,"hwm_kib":self.hwm_kib})
    }
}
fn memory() -> Memory {
    let status = fs::read_to_string("/proc/self/status").expect("Linux /proc metrics required");
    let field = |name: &str| -> u64 {
        status
            .lines()
            .find_map(|line| {
                let tail = line.strip_prefix(name)?;
                Some(tail.split_whitespace().next().unwrap().parse().unwrap())
            })
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    Memory {
        rss_kib: field("VmRSS:"),
        hwm_kib: field("VmHWM:"),
    }
}

#[test]
fn metrics_fixture_and_stage_semantics() {
    let _borrowed = Borrowed(set_borrowed_base(true));
    let (before, update, merged) = fixture(64);
    for warm in [false, true] {
        for enabled in [false, true] {
            let doc = load(&before, 9);
            if warm {
                warm_history(&doc);
            }
            let _mode = Enabled::new(enabled);
            reset_stats();
            doc.import(&update).unwrap();
            assert_eq!(stats().hits, u64::from(enabled));
            assert_eq!(stats().borrowed_base_proofs, u64::from(enabled && !warm));
            let oracle = load(&merged, 9);
            equivalent(&doc, &oracle);
            let reopened = LoroDoc::new();
            reopened.set_peer_id(9).unwrap();
            reset_stats();
            reopened.import(&merged).unwrap();
            assert_eq!(stats().hits, 0);
            equivalent(&reopened, &oracle);
        }
    }
}

#[test]
#[ignore = "standalone fixture process, not a measured worker"]
fn strider_borrowed_metrics_setup() {
    let size: usize = std::env::var("STRIDER_SIZE").unwrap().parse().unwrap();
    assert!([32768, 262144].contains(&size));
    let dir = std::env::var("STRIDER_FIXTURE").unwrap();
    let dir = Path::new(&dir);
    fs::create_dir(dir).unwrap();
    let (before, update, merged) = fixture(size);
    fs::write(dir.join("before.bin"), &before).unwrap();
    fs::write(dir.join("update.bin"), &update).unwrap();
    fs::write(dir.join("merged.bin"), &merged).unwrap();
    fs::write(dir.join("fixture.json"), serde_json::to_vec_pretty(&json!({
        "size":size,"shape":"I/I single base run, two strictly disjoint concurrent insert runs",
        "before_bytes":before.len(),"update_bytes":update.len(),"merged_snapshot_bytes":merged.len()
    })).unwrap()).unwrap();
}

#[test]
#[ignore = "one measured operation per fresh process; explicit runner only"]
fn strider_borrowed_metrics_worker() {
    let _borrowed = Borrowed(set_borrowed_base(true));
    let dir = std::env::var("STRIDER_FIXTURE").unwrap();
    let dir = Path::new(&dir);
    let output = std::env::var("STRIDER_OUTPUT").unwrap();
    let stage = std::env::var("STRIDER_STAGE").unwrap();
    assert!(["merge", "reopen"].contains(&stage.as_str()));
    let enabled = std::env::var("STRIDER_ENABLED").unwrap() == "1";
    let history = std::env::var("STRIDER_HISTORY").unwrap();
    assert!(["cold", "warm"].contains(&history.as_str()));
    let warm = history == "warm";
    let mut history_prewarm_ns = 0;
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("fixture.json")).unwrap()).unwrap();
    let size = metadata["size"].as_u64().unwrap() as usize;
    let _mode = Enabled::new(enabled);
    reset_stats();
    let (doc, input, setup_memory, outer_start) = if stage == "merge" {
        let before = fs::read(dir.join("before.bin")).unwrap();
        let doc = load(&before, 9);
        drop(before);
        let input = fs::read(dir.join("update.bin")).unwrap();
        if warm {
            history_prewarm_ns = warm_history(&doc);
        }
        (doc, input, memory(), None)
    } else {
        // Outer reopen starts before snapshot file read and document creation.
        // The baseline probe stays outside, not hidden inside the outer timer.
        let setup = memory();
        let outer = Instant::now();
        let input = fs::read(dir.join("merged.bin")).unwrap();
        let doc = LoroDoc::new();
        doc.set_peer_id(9).unwrap();
        (doc, input, setup, Some(outer))
    };
    let start = Instant::now();
    let imported = doc.import(&input);
    let import_ns = start.elapsed().as_nanos();
    // A snapshot may retain lazy text state. Do not call its import time alone
    // "usable reopen": separately include the first full text materialization.
    let materialized = if stage == "reopen" {
        Some(doc.get_text("t").to_string())
    } else {
        None
    };
    let import_and_materialize_ns = if stage == "reopen" {
        Some(start.elapsed().as_nanos())
    } else {
        None
    };
    let reopen_outer_ns = outer_start.map(|clock| clock.elapsed().as_nanos());
    let observed = stats();
    let materialized_bytes = materialized.as_ref().map(String::len);
    drop(materialized);
    // Reopen's primary natural usable-state timer is already stopped. Optional
    // history-ready state is a separate paid phase, not a faster cold reopen.
    if warm && stage == "reopen" {
        history_prewarm_ns = warm_history(&doc);
    }
    let history_ready_outer_ns = if warm && stage == "reopen" {
        outer_start.map(|clock| clock.elapsed().as_nanos())
    } else {
        reopen_outer_ns
    };
    let operation_memory = memory();
    drop(input);
    thread::sleep(Duration::from_millis(50));
    let rest_memory = memory();
    // All assertions, second-document oracle construction and snapshot export
    // follow the memory/timing window; they cannot inflate its recorded HWM.
    imported.unwrap();
    assert_eq!(observed.hits, u64::from(enabled && stage == "merge"));
    assert_eq!(
        observed.borrowed_base_proofs,
        u64::from(enabled && !warm && stage == "merge")
    );
    if let Some(len) = materialized_bytes {
        assert_eq!(len, size + 11);
    }
    let exported = doc.export(ExportMode::Snapshot).unwrap();
    let snapshot_bytes = exported.len();
    let snapshot_path = Path::new(&output).with_extension("snapshot.bin");
    fs::write(&snapshot_path, &exported).unwrap();
    let oracle_bytes = fs::read(dir.join("merged.bin")).unwrap();
    let oracle = load(&oracle_bytes, 9);
    equivalent(&doc, &oracle);
    let reopened = load(&exported, 9);
    equivalent(&reopened, &oracle);
    // Verify future conflicting edits against the original native path too.
    let branch = load(&oracle_bytes, 12);
    branch.get_text("t").insert(0, "branch").unwrap();
    branch.commit();
    let future_update = branch
        .export(ExportMode::updates(&oracle.oplog_vv()))
        .unwrap();
    for (target, use_candidate) in [(&doc, enabled), (&oracle, false)] {
        let _selection = Enabled::new(use_candidate);
        target.get_text("t").insert(0, "next").unwrap();
        target.commit();
        target.import(&future_update).unwrap();
    }
    equivalent(&doc, &oracle);
    fs::write(output, serde_json::to_vec_pretty(&json!({
        "schema":3,"size":size,"stage":stage,"enabled":enabled,"history":history,
        "history_prewarm_ns":history_prewarm_ns,"history_ready_outer_ns":history_ready_outer_ns,
        "import_ns":import_ns,"import_and_materialize_ns":import_and_materialize_ns,"reopen_outer_ns":reopen_outer_ns,
        "setup_memory_boundary":if stage=="merge" {"after before-snapshot load, update read and optional paid history prewarm"} else {"before snapshot file read and document creation"},
        "setup_memory":setup_memory.json(),"operation_memory":operation_memory.json(),"rest_memory":rest_memory.json(),
        "snapshot_bytes":snapshot_bytes,"snapshot_path":snapshot_path,"snapshot_exactly_equals_setup":exported==oracle_bytes,
        "attempts":observed.attempts,"hits":observed.hits,"unparsed_fallbacks":observed.unparsed_fallbacks,"borrowed_base_attempts":observed.borrowed_base_attempts,"borrowed_base_proofs":observed.borrowed_base_proofs,
        "oracle":"full native history, state, cursors, exported reload and future same-gap merge passed",
        "memory_notes":"Linux process RSS/HWM KiB; HWM includes setup and is not reset; rest after input/string drop and50ms sleep, retaining document and allocator pages. Not live allocations or operation-only peak."
    })).unwrap()).unwrap();
}

struct Borrowed(bool);
impl Drop for Borrowed {
    fn drop(&mut self) {
        set_borrowed_base(self.0);
    }
}
