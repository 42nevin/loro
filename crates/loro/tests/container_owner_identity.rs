use loro::{ContainerTrait, LoroDoc, LoroText};

#[test]
fn container_identity_distinguishes_reference_clone_fork_and_detached_handle() {
    let doc = LoroDoc::new();
    let text = doc.get_text("body");
    text.insert(0, "owner").unwrap();
    doc.commit();
    let alias = doc.clone();
    let fork = doc.fork_cow().unwrap();
    assert_eq!(doc.oplog_vv(), fork.oplog_vv());
    assert_eq!(text.id(), fork.get_text("body").id());
    assert!(text.is_attached_to(&doc));
    assert!(text.is_attached_to(&alias));
    assert!(!text.is_attached_to(&fork));
    assert!(!LoroText::new().is_attached_to(&doc));
    doc.delete_root_container(text.id());
    assert!(text.is_attached_to(&doc));
}
