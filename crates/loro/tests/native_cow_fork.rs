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

#[test]
fn styled_growth_keeps_held_forks_and_nested_containers_independent() {
    use std::collections::VecDeque;
    let doc = base();
    doc.get_text("text").mark(0..2, "bold", true).unwrap();
    doc.commit();
    let chunk = "界🙂a".repeat(1024);
    let mut held = VecDeque::new();
    for round in 0..128 {
        let branch = doc.fork_cow().unwrap();
        branch.set_peer_id(100 + round).unwrap();
        let source_delta = doc.get_text("text").to_delta();
        let source_value = doc.get_deep_value();
        let source_vv = doc.oplog_vv();
        // Warm style and value paths before each owner's independent write.
        assert_eq!(branch.get_text("text").to_delta(), source_delta);
        assert_eq!(branch.get_deep_value(), source_value);
        branch.get_text("text").unmark(0..2, "bold").unwrap();
        branch.get_text("text").mark(1..3, "italic", true).unwrap();
        let child = branch.get_map("meta").insert_container("private", LoroText::new()).unwrap();
        child.insert(0, "discarded nested container").unwrap();
        branch.get_text("text").insert(0, "loser").unwrap();
        branch.commit();
        assert_eq!(doc.get_text("text").to_delta(), source_delta);
        assert_eq!(doc.get_deep_value(), source_value);
        assert_eq!(doc.oplog_vv(), source_vv);
        let expected_delta = branch.get_text("text").to_delta();
        let expected_value = branch.get_deep_value();
        held.push_back((branch, expected_delta, expected_value));
        if held.len() > 8 { held.pop_front(); }
        let text = doc.get_text("text");
        text.insert(text.len_unicode(), &chunk).unwrap();
        text.mark(0..2, "bold", round % 2 == 0).unwrap();
        doc.commit();
        for (branch, delta, value) in &held {
            assert_eq!(&branch.get_text("text").to_delta(), delta);
            assert_eq!(&branch.get_deep_value(), value);
        }
        assert!(doc.get_map("meta").get("private").is_none());
    }
    drop(held);
    let oracle = LoroDoc::from_snapshot(&doc.export(ExportMode::snapshot()).unwrap()).unwrap();
    assert_eq!(doc.get_deep_value(), oracle.get_deep_value());
    assert_eq!(doc.get_text("text").to_delta(), oracle.get_text("text").to_delta());
    assert_eq!(doc.oplog_vv(), oracle.oplog_vv());
}

#[test]
fn shallow_nested_parent_paths_survive_cold_forks_and_branch_of_branch() {
    use loro::{ContainerTrait, LoroMap};
    let source = base();
    let parent = source.get_map("meta").insert_container("nested", LoroMap::new()).unwrap();
    let child = parent.insert_container("text", LoroText::new()).unwrap();
    child.insert(0, "old界🙂").unwrap();
    child.mark(0..3, "bold", true).unwrap();
    source.commit();
    let frontier = source.oplog_frontiers();
    source.get_text("text").insert(0, "after-root").unwrap();
    source.commit();
    let expected_path = source.get_path_to_container(&child.id()).unwrap();
    let shallow = LoroDoc::from_snapshot(&source.export(ExportMode::shallow_snapshot(&frontier)).unwrap()).unwrap();
    let fork = shallow.fork_cow().unwrap();
    let nested_fork = fork.fork_cow().unwrap();
    for doc in [&nested_fork, &fork, &shallow] {
        assert_eq!(doc.get_path_to_container(&child.id()).unwrap(), expected_path);
        assert_eq!(doc.get_deep_value(), source.get_deep_value());
        assert_eq!(doc.get_text(child.id()).to_delta(), child.to_delta());
    }
    let before = shallow.get_deep_value();
    nested_fork.get_text(child.id()).insert(0, "branch-only").unwrap();
    nested_fork.get_text(child.id()).unmark(0..3, "bold").unwrap();
    nested_fork.commit();
    assert_eq!(shallow.get_deep_value(), before);
    assert_eq!(fork.get_deep_value(), before);
    fork.import(&nested_fork.export(ExportMode::updates(&fork.oplog_vv())).unwrap()).unwrap();
    assert_eq!(fork.get_deep_value(), nested_fork.get_deep_value());
    assert_eq!(shallow.get_deep_value(), before);
    assert_eq!(fork.get_path_to_container(&child.id()).unwrap(), expected_path);
}
