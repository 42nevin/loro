#![cfg(feature = "shared_arena_experiment")]
use loro::{ContainerTrait, LoroDoc, LoroText};
fn contents(s: &loro::TextStateSnapshot) -> String {
    let mut out = String::new();
    for run in &s.runs { run.with_str("snapshot.test", |s| out.push_str(s)); }
    out
}
#[test]
fn current_state_snapshot_retains_live_runs_not_history() {
    assert!(LoroText::new().shared_text_snapshot().is_none());
    let doc = LoroDoc::new();
    let text = doc.get_text("body");
    text.insert(0, "A玉🙂B").unwrap(); doc.commit();
    let old = text.shared_text_snapshot().unwrap();
    assert_eq!(contents(&old), "A玉🙂B");
    assert_eq!(old.unicode_len, 4); assert_eq!(old.utf16_len, 5); assert_eq!(old.byte_len, 9);
    assert_eq!(old.container, text.id());
    text.delete(1, 1).unwrap(); text.insert(1, "X").unwrap(); doc.commit();
    let new = text.shared_text_snapshot().unwrap();
    assert_eq!(contents(&new), "AX🙂B");
    assert_ne!(old.version, new.version);
    assert_eq!(contents(&old), "A玉🙂B");
    drop(text); drop(doc);
    assert_eq!(contents(&new), "AX🙂B");
    assert_eq!(contents(&old), "A玉🙂B");
}
#[test]
fn reopened_and_merged_state_snapshots_match_independent_text() {
    let a=LoroDoc::new(); a.set_peer_id(1).unwrap();
    let t=a.get_text("body"); t.insert(0,"A玉🙂B").unwrap(); a.commit();
    let bytes=a.export(loro::ExportMode::Snapshot).unwrap();
    let b=LoroDoc::new(); b.import(&bytes).unwrap(); b.set_peer_id(2).unwrap();
    let bt=b.get_text("body");
    let retained=bt.shared_text_snapshot().unwrap(); assert_eq!(contents(&retained), t.to_string());
    bt.insert(2,"e\u{301}").unwrap(); b.commit();
    t.insert(2,"X").unwrap(); a.commit();
    a.import(&b.export(loro::ExportMode::all_updates()).unwrap()).unwrap();
    assert_eq!(contents(&t.shared_text_snapshot().unwrap()),t.to_string());
    assert_eq!(contents(&retained),"A玉🙂B");
    t.delete(0,t.len_unicode()).unwrap(); a.commit();
    let empty=t.shared_text_snapshot().unwrap(); assert_eq!(empty.byte_len,0); assert!(empty.runs.is_empty());
}
