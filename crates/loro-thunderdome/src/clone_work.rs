//! Opt-in observations, not an allocation or CPU-cost model.
use std::{cell::RefCell, collections::BTreeMap};

/// Counts for actual backing-vector clones of one arena value type.
#[derive(Clone, Debug, Default)]
pub struct ArenaCloneWork {
    /// Number of Arena::clone calls.
    pub clone_calls: usize,
    /// Entries passed to the original full backing Vec clone, not capacity.
    pub storage_slots: usize,
    /// Live entries copied by those calls.
    pub occupied_slots: usize,
    /// Vacant entries copied by those calls.
    pub vacant_slots: usize,
}

thread_local! {
    static WORK: RefCell<Option<BTreeMap<&'static str, ArenaCloneWork>>> =
        RefCell::new(None);
}

/// Begin observations for the current thread.
pub fn reset() {
    WORK.with(|slot| *slot.borrow_mut() = Some(BTreeMap::new()));
}

/// End observations and return the current thread counts.
pub fn take() -> BTreeMap<&'static str, ArenaCloneWork> {
    WORK.with(|slot| slot.borrow_mut().take().unwrap_or_default())
}

pub(crate) fn record<T>(storage: usize, occupied: usize) {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            let entry = work.entry(std::any::type_name::<T>()).or_default();
            entry.clone_calls = entry.clone_calls.saturating_add(1);
            entry.storage_slots = entry.storage_slots.saturating_add(storage);
            entry.occupied_slots = entry.occupied_slots.saturating_add(occupied);
            entry.vacant_slots = entry.vacant_slots.saturating_add(storage.saturating_sub(occupied));
        }
    });
}
