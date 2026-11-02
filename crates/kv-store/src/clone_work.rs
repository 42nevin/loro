//! Opt-in, current-thread observations of actual metadata clone operations.
//! Immutable Bytes references are not counted as payload byte copies.
use std::cell::RefCell;

#[derive(Debug, Default, Clone)]
pub struct Work {
    pub store_clones: usize,
    pub memtable_entries_cloned: usize,
    pub sstable_wrappers_cloned: usize,
    pub block_meta_entries_cloned: usize,
    pub block_cache_instances_created: usize,
}
thread_local! {
    static WORK: RefCell<Option<Work>> = const { RefCell::new(None) };
}
/// Begin an explicitly selected phase on this thread.
pub fn reset() {
    WORK.with(|slot| *slot.borrow_mut() = Some(Work::default()));
}
/// End the phase; subsequent operations are unobserved until reset.
pub fn take() -> Work {
    WORK.with(|slot| slot.borrow_mut().take().unwrap_or_default())
}
pub(crate) fn store_clone(entries: usize) {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.store_clones += 1;
            w.memtable_entries_cloned += entries;
        }
    });
}
pub(crate) fn table_clone() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.sstable_wrappers_cloned += 1;
        }
    });
}
pub(crate) fn meta_clone() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.block_meta_entries_cloned += 1;
        }
    });
}
pub(crate) fn cache_created() {
    WORK.with(|slot| {
        if let Some(w) = slot.borrow_mut().as_mut() {
            w.block_cache_instances_created += 1;
        }
    });
}
