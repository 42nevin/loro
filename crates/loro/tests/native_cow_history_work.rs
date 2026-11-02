//! Fixed visible state, varying retained history. Counters measure actual copy sites,
//! not cache hits or wall time. Native snapshots are setup/oracles only.
use loro::{ExportMode, LoroDoc};
use loro_internal::cow_work::{self, Work};

fn history(pairs: usize, cold: bool) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    let text = doc.get_text("text");
    text.insert(0, "A界🙂Z").unwrap();
    doc.get_map("untouched").insert("value", 19).unwrap();
    doc.commit();
    for _ in 0..pairs {
        text.insert(1, "historical界🙂payload").unwrap();
        doc.commit();
        text.delete(1, "historical界🙂payload".chars().count())
            .unwrap();
        doc.commit();
    }
    assert_eq!(text.to_string(), "A界🙂Z");
    if cold {
        LoroDoc::from_snapshot(&doc.export(ExportMode::snapshot()).unwrap()).unwrap()
    } else {
        // Explicitly warm current-state reads, outside all measured phases.
        let _ = doc.get_deep_value();
        let _ = text.to_delta();
        doc
    }
}

fn measured<T>(f: impl FnOnce() -> T) -> (T, Work) {
    cow_work::reset();
    let value = f();
    (value, cow_work::take())
}

#[test]
fn fixed_text_history_reports_copy_work_and_rejects_string_bulk_copy() {
    let mut observed = Vec::new();
    for cold in [false, true] {
        for pairs in [0, 8, 64] {
            let doc = history(pairs, cold);
            let original = doc.get_deep_value();
            let vv = doc.oplog_vv();
            let frontier = doc.state_frontiers();
            let (loser, work) = measured(|| doc.fork_cow().unwrap());
            observed.push((cold, pairs, "cancel_fork", work));
            loser.set_peer_id(7).unwrap();
            let (_, work) = measured(|| {
                loser.get_text("text").insert(1, "!").unwrap();
                loser.commit();
            });
            observed.push((cold, pairs, "cancel_first_mutation", work));
            assert_eq!(doc.get_deep_value(), original);
            assert_eq!(doc.oplog_vv(), vv);
            let (_, work) = measured(|| drop(loser));
            observed.push((cold, pairs, "cancel_drop", work));
            assert_eq!(doc.state_frontiers(), frontier);

            let (chosen, work) = measured(|| doc.fork_cow().unwrap());
            observed.push((cold, pairs, "selected_fork", work));
            chosen.set_peer_id(7).unwrap();
            let (_, work) = measured(|| {
                chosen.get_text("text").insert(1, "!").unwrap();
                chosen.commit();
            });
            observed.push((cold, pairs, "selected_first_mutation", work));
            assert_eq!(doc.get_deep_value(), original);
            let (update, work) = measured(|| chosen.export(ExportMode::updates(&vv)).unwrap());
            observed.push((cold, pairs, "selected_delta_export", work));
            let (_, work) = measured(|| doc.import(&update).unwrap());
            observed.push((cold, pairs, "publish_import", work));
            assert_eq!(doc.get_deep_value(), chosen.get_deep_value());
            assert_eq!(doc.oplog_vv(), chosen.oplog_vv());
            assert_eq!(doc.get_text("text").to_string(), "A!界🙂Z");
            let (next, work) = measured(|| doc.fork_cow().unwrap());
            observed.push((cold, pairs, "next_fork", work));
            next.set_peer_id(7).unwrap();
            let (_, work) = measured(|| {
                next.get_text("text").insert(1, "?").unwrap();
                next.commit();
            });
            observed.push((cold, pairs, "next_first_mutation", work));
            assert_eq!(doc.get_text("text").to_string(), "A!界🙂Z");
            assert_eq!(next.get_text("text").to_string(), "A?!界🙂Z");
            let oracle =
                LoroDoc::from_snapshot(&doc.export(ExportMode::snapshot()).unwrap()).unwrap();
            assert_eq!(oracle.get_deep_value(), doc.get_deep_value());
            assert_eq!(oracle.state_frontiers(), doc.state_frontiers());
            // Artifact exports occur only after every measured phase. These are
            // new candidate fixtures, never retroactive baseline samples.
            if let Some(root) = std::env::var_os("LORO_COW_FIXTURE_DIR") {
                let path =
                    std::path::PathBuf::from(root).join(format!("cold-{cold}-pairs-{pairs}"));
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(
                    path.join("published.snapshot"),
                    doc.export(ExportMode::snapshot()).unwrap(),
                )
                .unwrap();
                std::fs::write(
                    path.join("next-private.snapshot"),
                    next.export(ExportMode::snapshot()).unwrap(),
                )
                .unwrap();
                std::fs::write(path.join("selected.update"), &update).unwrap();
                std::fs::write(path.join("expected.txt"),
                    format!("Native Loro fixture; not a Turso database.\ncold={cold}\nhistory_pairs={pairs}\nbase=A界🙂Z\npublished=A!界🙂Z\nnext_private=A?!界🙂Z\npeer=7\nuntouched.value=19\n")).unwrap();
            }
        }
    }
    for (cold, pairs, phase, work) in &observed {
        eprintln!("KV_WORK fixture=history cold={cold} pairs={pairs} phase={phase} work={work:?}");
        eprintln!("COW_WORK cold={cold} pairs={pairs} phase={phase} metadata={:?} string_index_path_entries={} segment_visits={} segment_entries_cloned={} scalar_indexes_cloned={} dag_node_refs_cloned={} dag_node_values_cloned={}",
            work.metadata_entries_copied, work.string_index_entries_cloned, work.string_segment_visits, work.string_segment_entries_cloned, work.string_scalar_indexes_cloned, work.dag_node_refs_cloned, work.dag_node_values_cloned);
    }
    // This first-layer contract does NOT claim that DAG, KV/SSTable or every
    // history copy is eliminated. Preserve their observations independently.
    let bulk: usize = observed
        .iter()
        .map(|(_, _, _, work)| {
            ["string_unicode_bulk", "string_segments_bulk"]
                .iter()
                .map(|name| work.metadata_entries_copied.get(name).copied().unwrap_or(0))
                .sum::<usize>()
        })
        .sum();
    assert_eq!(
        bulk, 0,
        "ordinary branch phases must not clone complete retained-history string indexes"
    );
}

#[test]
fn fixed_two_peer_history_exposes_noncoalescing_metadata_shape() {
    let mut observations = Vec::new();
    for pairs in [0, 8, 64, 1024] {
        let doc = history(0, false);
        let remote = LoroDoc::from_snapshot(&doc.export(ExportMode::snapshot()).unwrap()).unwrap();
        remote.set_peer_id(8).unwrap();
        // Common seed cycle gives every case the same two native peers.
        remote.get_text("text").insert(1, "seed").unwrap();
        remote.commit();
        remote.get_text("text").delete(1, 4).unwrap();
        remote.commit();
        doc.import(&remote.export(ExportMode::updates(&doc.oplog_vv())).unwrap())
            .unwrap();
        for _ in 0..pairs {
            doc.get_text("text")
                .insert(1, "historical界🙂payload")
                .unwrap();
            doc.commit();
            remote
                .import(&doc.export(ExportMode::updates(&remote.oplog_vv())).unwrap())
                .unwrap();
            remote
                .get_text("text")
                .delete(1, "historical界🙂payload".chars().count())
                .unwrap();
            remote.commit();
            doc.import(&remote.export(ExportMode::updates(&doc.oplog_vv())).unwrap())
                .unwrap();
        }
        assert_eq!(doc.get_text("text").to_string(), "A界🙂Z");
        assert_eq!(doc.oplog_vv(), remote.oplog_vv());
        let original = doc.get_deep_value();
        let vv = doc.oplog_vv();
        let (branch, fork) = measured(|| doc.fork_cow().unwrap());
        branch.set_peer_id(7).unwrap();
        let (_, mutate) = measured(|| {
            branch.get_text("text").insert(1, "!").unwrap();
            branch.commit();
        });
        assert_eq!(doc.get_deep_value(), original);
        assert_eq!(doc.oplog_vv(), vv);
        assert_eq!(branch.get_text("text").to_string(), "A!界🙂Z");
        if let Some(root) = std::env::var_os("LORO_COW_FIXTURE_DIR") {
            let path = std::path::PathBuf::from(root).join(format!("two-peer-pairs-{pairs}"));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("base.snapshot"),
                doc.export(ExportMode::snapshot()).unwrap(),
            )
            .unwrap();
            std::fs::write(
                path.join("private.snapshot"),
                branch.export(ExportMode::snapshot()).unwrap(),
            )
            .unwrap();
            std::fs::write(path.join("expected.txt"), format!("New native Loro diagnostic; not Turso DB.\npeers=7,8\ncommon_peer8_seed_insert_delete=1\nadditional_alternating_pairs={pairs}\nbase=A界🙂Z\nprivate=A!界🙂Z\n")).unwrap();
        }
        observations.push((pairs, fork, mutate));
    }
    let dag_bulk: usize = observations
        .iter()
        .map(|(_, fork, mutate)| {
            fork.metadata_entries_copied
                .get("dag_nodes_bulk")
                .copied()
                .unwrap_or(0)
                + mutate
                    .metadata_entries_copied
                    .get("dag_nodes_bulk")
                    .copied()
                    .unwrap_or(0)
        })
        .sum();
    let parsed_blocks_bulk: usize = observations
        .iter()
        .map(|(_, fork, _)| {
            fork.metadata_entries_copied
                .get("change_blocks_bulk")
                .copied()
                .unwrap_or(0)
        })
        .sum();
    for (pairs, fork, mutate) in observations {
        eprintln!("KV_WORK fixture=two_peer pairs={pairs} phase=fork work={fork:?}");
        eprintln!("KV_WORK fixture=two_peer pairs={pairs} phase=mutation work={mutate:?}");
        eprintln!("BLOCK_WORK pairs={pairs} fork_refs={} fork_values={} fork_headers={} fork_header_entries={} mutation_refs={} mutation_values={} mutation_headers={} mutation_header_entries={}", fork.change_block_references_cloned, fork.change_block_values_cloned, fork.change_header_values_cloned, fork.change_header_entries_cloned, mutate.change_block_references_cloned, mutate.change_block_values_cloned, mutate.change_header_values_cloned, mutate.change_header_entries_cloned);
        eprintln!("TWO_PEER_WORK pairs={pairs} fork_metadata={:?} mutation_metadata={:?} fork_index_clones={} fork_segment_clones={} mutation_index_clones={} mutation_segment_clones={} fork_dag_refs={} fork_dag_values={} mutation_dag_refs={} mutation_dag_values={}", fork.metadata_entries_copied, mutate.metadata_entries_copied, fork.string_index_entries_cloned, fork.string_segment_entries_cloned, mutate.string_index_entries_cloned, mutate.string_segment_entries_cloned, fork.dag_node_refs_cloned, fork.dag_node_values_cloned, mutate.dag_node_refs_cloned, mutate.dag_node_values_cloned);
    }
    assert_eq!(
        dag_bulk, 0,
        "ordinary branch phases must not clone the complete retained DAG"
    );
    assert_eq!(
        parsed_blocks_bulk, 0,
        "fork must not clone every retained parsed-block wrapper"
    );
}

#[test]
fn deleted_fragment_highwater_does_not_sweep_vacant_arena_storage() {
    let mut observations = Vec::new();
    for cold in [false, true] {
        for peak in [0, 64, 1024] {
            let source = history(0, false);
            for _ in 0..peak {
                // Reverse native ID order prevents one adjacent mergeable RLE span.
                source.get_text("text").insert(1, "x").unwrap();
                source.commit();
            }
            if peak != 0 {
                source.get_text("text").delete(1, peak).unwrap();
                source.commit();
            }
            assert_eq!(source.get_text("text").to_string(), "A界🙂Z");
            let doc = if cold {
                LoroDoc::from_snapshot(&source.export(ExportMode::snapshot()).unwrap()).unwrap()
            } else {
                source
            };
            let original = doc.get_deep_value();
            let vv = doc.oplog_vv();
            let (branch, work) = measured(|| doc.fork_cow().unwrap());
            observations.push((cold, peak, "fork", work));
            branch.set_peer_id(7).unwrap();
            let (_, work) = measured(|| {
                branch.get_text("text").insert(1, "!").unwrap();
                branch.commit();
            });
            observations.push((cold, peak, "first_mutation", work));
            assert_eq!(doc.get_deep_value(), original);
            assert_eq!(doc.oplog_vv(), vv);
            let (update, work) = measured(|| branch.export(ExportMode::updates(&vv)).unwrap());
            observations.push((cold, peak, "delta_export", work));
            let (_, work) = measured(|| doc.import(&update).unwrap());
            observations.push((cold, peak, "publish_import", work));
            assert_eq!(doc.get_deep_value(), branch.get_deep_value());
            assert_eq!(doc.oplog_vv(), branch.oplog_vv());
            let (next, work) = measured(|| doc.fork_cow().unwrap());
            observations.push((cold, peak, "next_fork", work));
            next.set_peer_id(7).unwrap();
            let (_, work) = measured(|| {
                next.get_text("text").insert(1, "?").unwrap();
                next.commit();
            });
            observations.push((cold, peak, "next_mutation", work));
            assert_eq!(doc.get_text("text").to_string(), "A!界🙂Z");
            assert_eq!(next.get_text("text").to_string(), "A?!界🙂Z");
            if let Some(root) = std::env::var_os("LORO_COW_FIXTURE_DIR") {
                let path = std::path::PathBuf::from(root)
                    .join(format!("highwater-cold-{cold}-peak-{peak}"));
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(
                    path.join("published.snapshot"),
                    doc.export(ExportMode::snapshot()).unwrap(),
                )
                .unwrap();
                std::fs::write(
                    path.join("private.snapshot"),
                    next.export(ExportMode::snapshot()).unwrap(),
                )
                .unwrap();
                std::fs::write(path.join("selected.update"), &update).unwrap();
                std::fs::write(path.join("expected.txt"), format!("Native Loro, not TursoDB.\ncold={cold}\npast_peak_fragments={peak}\nsetup=reverse insert atUnicode1, commit each, then delete entire temporary range\npublished=A!界🙂Z\nprivate=A?!界🙂Z\n")).unwrap();
            }
        }
    }
    let mut vacant_bulk = 0;
    for (cold, peak, phase, work) in observations {
        eprintln!("KV_WORK fixture=highwater cold={cold} peak={peak} phase={phase} work={work:?}");
        assert!(
            work.arena_clone_observer_enabled,
            "dependency observer must be active"
        );
        eprintln!(
            "ARENA_WORK cold={cold} peak={peak} phase={phase} full_vec_clones={:?} page_clones={:?} page_references_cloned={}",
            work.arena_clones, work.arena_page_clones, work.arena_page_references_cloned
        );
        // Only actual full backing Vec clone sites contribute. This does not
        // outlaw bounded occupied-state/path copies in a future representation.
        vacant_bulk += work
            .arena_clones
            .values()
            .map(|value| value.vacant_slots)
            .sum::<usize>();
    }
    assert_eq!(
        vacant_bulk, 0,
        "ordinary branch work must not sweep historical vacant arena slots"
    );
}

#[test]
fn deleted_nested_history_does_not_bulk_copy_shared_arena_tables() {
    use loro::{ContainerTrait, LoroMap, LoroText};
    let mut observations = Vec::new();
    let mut expected_visible = None;
    for count in [0, 8, 64] {
        let source = history(0, false);
        let root = source.get_map("untouched");
        let mut deleted = Vec::new();
        for i in 0..count {
            let nested = root.insert_container("temporary", LoroMap::new()).unwrap();
            nested
                .insert("value", format!("retained-value-{i}"))
                .unwrap();
            let text = nested.insert_container("text", LoroText::new()).unwrap();
            text.insert(0, "historical界🙂").unwrap();
            source.commit();
            root.delete("temporary").unwrap();
            source.commit();
            assert!(text.is_deleted());
            deleted.push(text);
        }
        let visible = source.get_deep_value();
        if let Some(expected) = &expected_visible {
            assert_eq!(&visible, expected);
        } else {
            expected_visible = Some(visible.clone());
        }
        let vv = source.oplog_vv();
        assert_eq!(vv.len(), 1, "peer cardinality must remain fixed");
        let (branch, fork) = measured(|| source.fork_cow().unwrap());
        branch.set_peer_id(7).unwrap();
        let (_, mutation) = measured(|| {
            branch.get_text("text").insert(1, "!").unwrap();
            branch.commit();
        });
        assert_eq!(source.get_deep_value(), visible);
        assert_eq!(source.oplog_vv(), vv);
        assert_eq!(branch.get_text("text").to_string(), "A!界🙂Z");
        assert_eq!(source.get_text("text").to_string(), "A界🙂Z");
        for old in &deleted {
            assert!(old.is_deleted());
            assert!(branch.get_text(old.id()).is_deleted());
            assert!(source.get_path_to_container(&old.id()).is_none());
            assert!(branch.get_path_to_container(&old.id()).is_none());
        }
        let update = branch.export(ExportMode::updates(&vv)).unwrap();
        source.import(&update).unwrap();
        assert_eq!(source.get_deep_value(), branch.get_deep_value());
        assert_eq!(source.oplog_vv(), branch.oplog_vv());
        if let Some(dir) = std::env::var_os("LORO_COW_FIXTURE_DIR") {
            let path = std::path::PathBuf::from(dir).join(format!("deleted-nested-{count}"));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("published.snapshot"),
                source.export(ExportMode::snapshot()).unwrap(),
            )
            .unwrap();
            std::fs::write(path.join("selected.update"), update).unwrap();
            std::fs::write(path.join("expected.txt"), format!("New native Loro fixture, not TursoDB.\ndeleted_nested_containers={count}\npeer=7\nroot_text=A!界🙂Z\nroot_untouched.value=19\nno visible temporary child\n")).unwrap();
        }
        observations.push((count, fork, mutation));
    }
    let mut arena_bulk = 0;
    let mut slots_bulk = 0;
    for (count, fork, mutation) in observations {
        eprintln!("SHARED_ARENA_WORK deleted_pairs={count} fork={fork:?} mutation={mutation:?}");
        for work in [&fork, &mutation] {
            slots_bulk += work
                .metadata_entries_copied
                .get("container_slots_bulk")
                .copied()
                .unwrap_or(0);
            arena_bulk += work
                .metadata_entries_copied
                .get("arena_container_ids_bulk")
                .copied()
                .unwrap_or(0);
            arena_bulk += work
                .metadata_entries_copied
                .get("arena_values_bulk")
                .copied()
                .unwrap_or(0);
        }
    }
    assert_eq!(
        arena_bulk, 0,
        "ordinary fork must not bulk copy retained SharedArena ID/value tables"
    );
    assert_eq!(
        slots_bulk, 0,
        "ordinary fork must not sweep all retained container slots"
    );
}
#[test]
fn late_root_first_edit_reports_actual_sparse_initialization() {
    use loro::{LoroMap, LoroText};
    let mut expected_visible = None;
    let mut dense_slots_initialized = 0;
    let mut point_edit_enumeration_entries = 0;
    let mut immutable_block_metadata_copies = 0;
    for count in [0, 8, 64] {
        let setup = history(0, false);
        let root = setup.get_map("untouched");
        for i in 0..count {
            let nested = root.insert_container("temporary", LoroMap::new()).unwrap();
            nested.insert("value", format!("old-{i}")).unwrap();
            nested
                .insert_container("text", LoroText::new())
                .unwrap()
                .insert(0, "old")
                .unwrap();
            setup.commit();
            root.delete("temporary").unwrap();
            setup.commit();
        }
        setup.get_text("late").insert(0, "A界🙂Z").unwrap();
        setup.commit();
        let visible = setup.get_deep_value();
        if let Some(expected) = &expected_visible {
            assert_eq!(&visible, expected);
        } else {
            expected_visible = Some(visible.clone());
        }
        let snapshot = setup.export(ExportMode::snapshot()).unwrap();
        let mut warm = Some(setup);
        for cold in [false, true] {
            // Do not materialize cold current values before the measured first edit.
            let source = if cold {
                LoroDoc::from_snapshot(&snapshot).unwrap()
            } else {
                warm.take().unwrap()
            };
            let vv = source.oplog_vv();
            assert_eq!(vv.len(), 1);
            let (branch, fork) = measured(|| source.fork_cow().unwrap());
            branch.set_peer_id(7).unwrap();
            let (_, mutation) = measured(|| {
                branch.get_text("late").insert(1, "!").unwrap();
                branch.commit();
            });
            assert!(fork.kv_clone_observer_enabled);
            immutable_block_metadata_copies += fork.kv_block_meta_entries_cloned
                + mutation.kv_block_meta_entries_cloned;
            dense_slots_initialized += fork.container_sparse_slots_initialized
                + mutation.container_sparse_slots_initialized;
            point_edit_enumeration_entries += fork.container_slot_enumeration_entries
                + mutation.container_slot_enumeration_entries;
            assert_eq!(source.get_deep_value(), visible);
            assert_eq!(source.oplog_vv(), vv);
            assert_eq!(branch.get_text("late").to_string(), "A!界🙂Z");
            let update = branch.export(ExportMode::updates(&vv)).unwrap();
            source.import(&update).unwrap();
            assert_eq!(source.get_deep_value(), branch.get_deep_value());
            eprintln!("LATE_ROOT_WORK cold={cold} deleted_pairs={count} fork={fork:?} mutation={mutation:?}");
            if let Some(dir) = std::env::var_os("LORO_COW_FIXTURE_DIR") {
                let path = std::path::PathBuf::from(dir)
                    .join(format!("late-root-cold-{cold}-deleted-{count}"));
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("setup.snapshot"), &snapshot).unwrap();
                std::fs::write(
                    path.join("published.snapshot"),
                    source.export(ExportMode::snapshot()).unwrap(),
                )
                .unwrap();
                std::fs::write(path.join("selected.update"), update).unwrap();
                std::fs::write(path.join("expected.txt"), format!("Native diagnostic, not TursoDB.\ncold={cold}\ndeleted_nested_pairs={count}\nroot text=A界🙂Z\nroot late=A!界🙂Z\nuntouched.value=19\n")).unwrap();
            }
        }
    }
    assert_eq!(
        dense_slots_initialized, 0,
        "ordinary first edit must not initialize dense gaps up to a historical container index"
    );
    assert_eq!(
        point_edit_enumeration_entries, 0,
        "point fork/edit must not enumerate all container slots"
    );
    assert_eq!(
        immutable_block_metadata_copies, 0,
        "fork must share immutable SSTable block metadata, not clone every retained entry"
    );
}
