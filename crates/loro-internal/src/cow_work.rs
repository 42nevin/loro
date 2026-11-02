//! Opt-in test-thread observations of actual native branch copy sites.
//! This is a partial copy census, not allocator accounting or a CPU bound.
use std::{cell::RefCell, collections::BTreeMap};

#[derive(Default, Debug, Clone)]
pub struct Work {
    pub kv_clone_observer_enabled: bool,
    pub kv_store_clones: usize,
    pub kv_memtable_entries_cloned: usize,
    pub kv_sstable_wrappers_cloned: usize,
    pub kv_block_meta_entries_cloned: usize,
    pub kv_block_cache_instances_created: usize,
    pub container_slot_entries_inserted: usize,
    pub container_slot_enumeration_entries: usize,
    pub container_fork_cache_queue_visits: usize,
    pub container_fork_cache_slots_reset: usize,
    pub container_slot_refs_cloned: usize,
    pub container_slot_same_owner_detaches: usize,
    pub container_slot_foreign_owner_rebinds: usize,
    pub container_sparse_slots_initialized: usize,
    pub arena_id_entries_cloned: usize,
    pub arena_value_entries_cloned: usize,
    /// Explicit contiguous callback gathers, separate from persistent path copies.
    pub arena_callback_entries_materialized: usize,
    pub change_block_values_cloned: usize,
    pub change_block_references_cloned: usize,
    pub change_header_values_cloned: usize,
    /// Base header array entries; nested frontier/immutable payload costs are not bytes.
    pub change_header_entries_cloned: usize,
    /// False when dependency-level instrumentation is not compiled in.
    pub arena_clone_observer_enabled: bool,
    /// Actual full backing Vec clone work, grouped by node value type.
    pub arena_clones: BTreeMap<&'static str, ArenaCloneWork>,
    /// Fixed-page detach work, separate from full backing-vector clones.
    pub arena_page_clones: BTreeMap<&'static str, ArenaCloneWork>,
    pub arena_page_references_cloned: usize,
    /// Entries copied by the named whole-collection clone operation.
    /// Distinct collections may refer to the same logical historical operation.
    pub metadata_entries_copied: BTreeMap<&'static str, usize>,
    /// Actual Index::clone calls, including persistent-tree leaf/path copying.
    pub string_index_entries_cloned: usize,
    pub string_scalar_indexes_cloned: usize,
    pub string_segment_entries_cloned: usize,
    pub dag_node_refs_cloned: usize,
    pub dag_node_values_cloned: usize,
    /// Segment entries inspected by lookup, cross-segment reads, or rollback.
    pub string_segment_visits: usize,
}
#[derive(Default, Debug, Clone)]
pub struct ArenaCloneWork {
    pub clone_calls: usize,
    pub storage_slots: usize,
    pub occupied_slots: usize,
    pub vacant_slots: usize,
}
thread_local! {
    static WORK: RefCell<Option<Work>> = const { RefCell::new(None) };
}
/// Begin one explicitly selected phase, discarding earlier observations.
pub fn reset() {
    #[cfg(feature = "test_utils")]
    {
        loro_kv_store::clone_work::reset();
        generic_btree::clone_work::reset();
        generic_btree::storage::page_work::reset();
    }
    WORK.with(|slot| *slot.borrow_mut() = Some(Work::default()));
}
/// End the phase. Subsequent operations are not observed until reset.
pub fn take() -> Work {
    let work = WORK.with(|slot| slot.borrow_mut().take().unwrap_or_default());
    #[cfg(feature = "test_utils")]
    {
        let mut work = work;
        let kv = loro_kv_store::clone_work::take();
        work.kv_clone_observer_enabled = true;
        work.kv_store_clones = kv.store_clones;
        work.kv_memtable_entries_cloned = kv.memtable_entries_cloned;
        work.kv_sstable_wrappers_cloned = kv.sstable_wrappers_cloned;
        work.kv_block_meta_entries_cloned = kv.block_meta_entries_cloned;
        work.kv_block_cache_instances_created = kv.block_cache_instances_created;
        work.arena_clone_observer_enabled = true;
        let pages = generic_btree::storage::page_work::take();
        work.arena_page_references_cloned = pages.references;
        work.arena_page_clones = pages
            .by_type
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    ArenaCloneWork {
                        clone_calls: value.pages,
                        storage_slots: value.slots,
                        occupied_slots: value.occupied,
                        vacant_slots: value.vacant,
                    },
                )
            })
            .collect();
        work.arena_clones = generic_btree::clone_work::take()
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    ArenaCloneWork {
                        clone_calls: value.clone_calls,
                        storage_slots: value.storage_slots,
                        occupied_slots: value.occupied_slots,
                        vacant_slots: value.vacant_slots,
                    },
                )
            })
            .collect();
        work
    }
    #[cfg(not(feature = "test_utils"))]
    work
}

pub(crate) fn index_clone() {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            work.string_index_entries_cloned += 1;
        }
    });
}

pub(crate) fn segment_visit() {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            work.string_segment_visits += 1;
        }
    });
}

pub(crate) fn scalar_index_clone() {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            work.string_scalar_indexes_cloned += 1;
        }
    });
}
pub(crate) fn segment_clone() {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            work.string_segment_entries_cloned += 1;
        }
    });
}

pub(crate) fn dag_node_ref_clone() {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            work.dag_node_refs_cloned += 1;
        }
    });
}
pub(crate) fn dag_node_value_clone() {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            work.dag_node_values_cloned += 1;
        }
    });
}

pub(crate) fn change_block_clone() {
    WORK.with(|w| {
        if let Some(w) = w.borrow_mut().as_mut() {
            w.change_block_values_cloned += 1;
        }
    });
}
pub(crate) fn change_header_clone(entries: usize) {
    WORK.with(|w| {
        if let Some(w) = w.borrow_mut().as_mut() {
            w.change_header_values_cloned += 1;
            w.change_header_entries_cloned += entries;
        }
    });
}

pub(crate) fn change_block_ref_clone() {
    WORK.with(|w| {
        if let Some(w) = w.borrow_mut().as_mut() {
            w.change_block_references_cloned += 1;
        }
    });
}

pub(crate) fn arena_id_clone() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.arena_id_entries_cloned += 1;
        }
    });
}
pub(crate) fn arena_value_clone() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.arena_value_entries_cloned += 1;
        }
    });
}
pub(crate) fn arena_callback_materialized(entries: usize) {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.arena_callback_entries_materialized += entries;
        }
    });
}

pub(crate) fn container_slot_ref_clone() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_slot_refs_cloned += 1;
        }
    });
}

pub(crate) fn container_slot_same_detach() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_slot_same_owner_detaches += 1;
        }
    });
}

pub(crate) fn container_slot_foreign_rebind() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_slot_foreign_owner_rebinds += 1;
        }
    });
}

pub(crate) fn container_fork_cache_visit() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_fork_cache_queue_visits += 1;
        }
    });
}

pub(crate) fn container_fork_cache_reset() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_fork_cache_slots_reset += 1;
        }
    });
}

pub(crate) fn container_slot_inserted() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_slot_entries_inserted += 1;
        }
    });
}
pub(crate) fn container_slot_enumeration(entries: usize) {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.container_slot_enumeration_entries += entries;
        }
    });
}
