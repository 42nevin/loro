//! Native branch ownership controls. Snapshot import is a test oracle only.
use loro::{ExportMode, LoroDoc, LoroText};

fn base() -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    doc.get_text("text").insert(0, "A界🙂Z").unwrap();
    doc.get_map("meta").insert("base", 1).unwrap();
    doc.commit();
    doc
}

#[test]
fn alternatives_and_abandonment_do_not_mutate_the_base() {
    let doc = base();
    let value = doc.get_deep_value();
    let vv = doc.oplog_vv();
    let frontiers = doc.state_frontiers();
    let first = doc.fork_cow().unwrap();
    let loser = doc.fork_cow().unwrap();
    // Deliberately overlapping op IDs: only the selected update may be imported.
    first.set_peer_id(7).unwrap();
    loser.set_peer_id(7).unwrap();
    first.get_text("text").insert(2, "accepted").unwrap();
    loser.get_text("text").delete(0, 2).unwrap();
    let child = loser
        .get_map("meta")
        .insert_container("private", LoroText::new())
        .unwrap();
    child.insert(0, "branch-only").unwrap();
    first.commit();
    loser.commit();
    assert_eq!(doc.get_deep_value(), value);
    assert_eq!(doc.oplog_vv(), vv);
    assert_eq!(doc.state_frontiers(), frontiers);
    let preview = doc.fork_cow().unwrap();
    drop(loser);
    doc.import(&first.export(ExportMode::updates(&vv)).unwrap())
        .unwrap();
    assert_eq!(doc.get_deep_value(), first.get_deep_value());
    assert_eq!(preview.get_deep_value(), value);
    assert_eq!(preview.oplog_vv(), vv);
    assert_eq!(preview.state_frontiers(), frontiers);
    assert!(doc.get_map("meta").get("private").is_none());
    let oracle = LoroDoc::new();
    oracle
        .import(&doc.export(ExportMode::snapshot()).unwrap())
        .unwrap();
    let next = doc.fork_cow().unwrap();
    assert_eq!(next.get_deep_value(), oracle.get_deep_value());
    assert_eq!(next.oplog_vv(), oracle.oplog_vv());
    assert_eq!(next.state_frontiers(), oracle.state_frontiers());
}

#[test]
fn native_fork_keeps_shallow_history_and_rejects_detached_views() {
    let source = base();
    let shallow = LoroDoc::new();
    shallow
        .import(
            &source
                .export(ExportMode::shallow_snapshot(&source.oplog_frontiers()))
                .unwrap(),
        )
        .unwrap();
    let fork = shallow.fork_cow().unwrap();
    assert_eq!(fork.get_deep_value(), shallow.get_deep_value());
    assert_eq!(fork.oplog_vv(), shallow.oplog_vv());
    fork.get_text("text").insert(4, "!").unwrap();
    fork.commit();
    shallow
        .import(
            &fork
                .export(ExportMode::updates(&shallow.oplog_vv()))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(fork.get_deep_value(), shallow.get_deep_value());
    source.detach();
    assert!(source.fork_cow().is_err());
}
