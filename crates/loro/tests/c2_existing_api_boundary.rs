//! Existing API fallback control, NOT lazy historical storage or app acceptance.
use loro::{ContainerTrait, ExportMode, LoroDoc, LoroError, LoroText, UndoManager, VersionVector};
fn load(bytes:&[u8])->LoroDoc {let d=LoroDoc::new();d.import(bytes).unwrap();d}
#[test]
fn compact_edit_old_dependency_and_new_full_owner_fallback() {
    let mut reports=Vec::new();
    for nested in [false,true] {for state_only in [false,true] {
        let main=LoroDoc::new();main.set_peer_id(1).unwrap();
        main.get_text("text").insert(0,"a玉🙂b").unwrap();
        let child=main.get_map("parent").insert_container("child",LoroText::new()).unwrap();
        child.insert(0,"old-child").unwrap();main.commit();
        let child_id=child.id();let base_vv=main.oplog_vv();let old_frontiers=main.oplog_frontiers();
        let base=main.export(ExportMode::Snapshot).unwrap();
        let offline=load(&base);offline.set_peer_id(2).unwrap();
        if nested {offline.get_text(child_id.clone()).insert(1,"OFFLINE").unwrap();}
        else {offline.get_text("text").insert(2,"OFFLINE").unwrap();}
        offline.commit();let old_update=offline.export(ExportMode::updates(&base_vv)).unwrap();
        // Main deletes exactly the identities addressed by the offline branch.
        main.get_text("text").delete(1,2).unwrap();main.get_map("parent").delete("child").unwrap();main.commit();
        for i in 0..8 {main.set_peer_id(10+i).unwrap();main.get_text("text").insert(1,"x").unwrap();main.commit();main.get_text("text").delete(1,1).unwrap();main.commit();}
        let cut=main.oplog_frontiers();let cut_vv=main.oplog_vv();
        let full=main.export(ExportMode::Snapshot).unwrap();
        let main_tail=main.export(ExportMode::updates(&base_vv)).unwrap();
        let compact=main.export(if state_only {ExportMode::state_only(None)}else{ExportMode::shallow_snapshot(&cut)}).unwrap();
        let accepted=load(&compact);assert!(accepted.is_shallow());
        let boundary=accepted.shallow_since_frontiers();
        assert_eq!(accepted.get_deep_value(),main.get_deep_value());assert_eq!(accepted.oplog_vv(),cut_vv);
        assert!(!accepted.has_container(&child_id));
        assert!(matches!(accepted.checkout(&old_frontiers),Err(LoroError::SwitchToVersionBeforeShallowRoot)));
        accepted.set_peer_id(99).unwrap();
        let mut undo=UndoManager::new(&accepted);
        accepted.get_text("text").insert(0,"LOCAL").unwrap();accepted.commit();undo.record_new_checkpoint().unwrap();
        let edited=accepted.get_deep_value();undo.undo().unwrap();assert_eq!(accepted.get_deep_value(),main.get_deep_value());
        undo.redo().unwrap();assert_eq!(accepted.get_deep_value(),edited);
        let local=accepted.export(ExportMode::updates(&cut_vv)).unwrap();
        let before=accepted.get_deep_value();let vv=accepted.oplog_vv();
        let candidate=accepted.fork_cow().unwrap();
        let err=candidate.import(&old_update).unwrap_err();
        assert!(matches!(err,LoroError::ImportUpdatesThatDependsOnOutdatedVersion));drop(candidate);
        assert_eq!(accepted.get_deep_value(),before);assert_eq!(accepted.oplog_vv(),vv);
        // Reimport full history into the SAME shallow object does not backfill it.
        accepted.import(&full).unwrap();assert!(accepted.is_shallow());assert_eq!(accepted.shallow_since_frontiers(),boundary);
        assert_eq!(accepted.get_deep_value(),before);assert_eq!(accepted.oplog_vv(),vv);
        assert!(matches!(accepted.fork_cow().unwrap().import(&old_update),Err(LoroError::ImportUpdatesThatDependsOnOutdatedVersion)));
        // Explicit demand fallback creates a NEW full owner and consumes archive.
        let rebuilt=load(&full);rebuilt.import(&local).unwrap();rebuilt.import(&old_update).unwrap();
        assert!(!rebuilt.is_shallow());
        // Oracle starts at earlier V0 and merges in different order, not from fallback snapshot.
        let oracle=load(&base);oracle.import(&old_update).unwrap();oracle.import(&main_tail).unwrap();oracle.import(&local).unwrap();
        assert_eq!(rebuilt.get_deep_value(),oracle.get_deep_value());assert_eq!(rebuilt.oplog_vv(),oracle.oplog_vv());
        assert_eq!(rebuilt.oplog_frontiers(),oracle.oplog_frontiers());
        assert_eq!(serde_json::to_value(rebuilt.export_json_updates_without_peer_compression(&VersionVector::default(),&rebuilt.oplog_vv())).unwrap(),
                   serde_json::to_value(oracle.export_json_updates_without_peer_compression(&VersionVector::default(),&oracle.oplog_vv())).unwrap());
        let value=rebuilt.get_deep_value();rebuilt.import(&old_update).unwrap();rebuilt.import(&local).unwrap();assert_eq!(rebuilt.get_deep_value(),value);
        reports.push(serde_json::json!({"case":if nested {"deleted_nested_text"}else{"deleted_character_anchor"},
            "arm":if state_only {"state_only"}else{"shallow"},"full_bytes":full.len(),"compact_bytes":compact.len(),
            "boundary":format!("{boundary:?}"),"old_update_rejected":true,"cow_accepted_unchanged":true,
            "same_object_backfilled":false,"new_full_owner_matches_independent_oracle":true,
            "new_undo_redo":true,"pre_root_checkout_rejected":true,"true_lazy_history":false}));
    }}
    println!("C2_EXISTING_API {}",serde_json::to_string(&reports).unwrap());
    if let Ok(path)=std::env::var("C2_BOUNDARY_OUTPUT") {std::fs::write(path,serde_json::to_vec_pretty(&reports).unwrap()).unwrap();}
}
