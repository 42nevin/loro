use crate::{
    arena::SharedArena,
    configure::Configure,
    container::idx::ContainerIdx,
    state::{container_store::FRONTIERS_KEY, ContainerCreationContext, ContainerState},
    utils::kv_wrapper::KvWrapper,
    version::Frontiers,
};
use bytes::Bytes;
use loro_common::{ContainerID, ContainerType, LoroResult, LoroValue};
use rustc_hash::{FxHashSet, FxHasher};
use std::hash::BuildHasherDefault;
use std::{collections::VecDeque, sync::Arc};

use super::ContainerWrapper;

// Slots share immutable owner-bound inputs. Every state/value/decode access first
// makes the selected slot private and binds it to this store, including after the
// original owner is dropped. Metadata-only reads never expose a State reference.
type SlotMap = im::HashMap<usize, StoreSlot, BuildHasherDefault<FxHasher>>;
struct StoreOwner;
struct StoreSlot {
    owner: Arc<StoreOwner>,
    wrapper: Arc<ContainerWrapper>,
}
impl Clone for StoreSlot {
    fn clone(&self) -> Self {
        #[cfg(any(test, feature = "test_utils"))]
        crate::cow_work::container_slot_ref_clone();
        Self {
            owner: self.owner.clone(),
            wrapper: self.wrapper.clone(),
        }
    }
}
impl StoreSlot {
    fn make_mut<'a>(
        &'a mut self,
        owner: &Arc<StoreOwner>,
        config: &Configure,
    ) -> &'a mut ContainerWrapper {
        // The primary map's mutable path has detached before this check.
        if !Arc::ptr_eq(&self.owner, owner) {
            let private = self.wrapper.fork_cow(config);
            #[cfg(any(test, feature = "test_utils"))]
            crate::cow_work::container_slot_foreign_rebind();
            self.wrapper = Arc::new(private);
            self.owner = owner.clone();
        } else if Arc::get_mut(&mut self.wrapper).is_none() {
            let private = self.wrapper.clone_in_owner();
            #[cfg(any(test, feature = "test_utils"))]
            crate::cow_work::container_slot_same_detach();
            self.wrapper = Arc::new(private);
        }
        Arc::get_mut(&mut self.wrapper).expect("selected wrapper is private")
    }
}

/// Upper bound on how many lazy containers keep their decoded value cached in
/// memory for the read paths (`map_get`, `list_get`, text reads, ...).
///
/// Reading a container through a handler decodes its value into the wrapper
/// once so repeated reads of the same container are cheap. Without a bound, a
/// container-by-container walk over an imported document pins O(containers
/// ever read) of memory until the doc is dropped — about 4 KB per container,
/// which traps wasm32 at the 4 GiB limit around one million containers
/// (loro-dev/loro#1092). Evicted wrappers are re-created from the KV store on
/// the next read, so eviction only costs a re-decode. The queue is a
/// second-chance FIFO: a container whose cached value is hit again survives
/// one eviction pass, which keeps ancestors hot during deep walks.
#[cfg(not(test))]
const MAX_CACHED_CONTAINER_VALUES: usize = 2048;
/// Tests use a small bound so exercising eviction does not require building
/// thousands of containers.
#[cfg(test)]
pub(super) const MAX_CACHED_CONTAINER_VALUES: usize = 16;

/// The invariants about this struct:
///
/// - `kv` is either the same or older than `store`.
/// - `store` is a cache over `kv`: every container in `kv` is either present in
///   `store` or was evicted from it by the bounded decoded-value cache
///   ([`MAX_CACHED_CONTAINER_VALUES`]). Evicted entries are re-created from
///   `kv` on the next access, so lookups must always fall back to `kv` on a
///   `store` miss, regardless of `load_state`.
/// - `load_state == AllLoaded` means every `kv` entry was materialized into
///   `store` at some point; it does NOT mean `store` is still complete. Once
///   `evicted_since_full_load` is set, `load_all()` must re-scan `kv`.
///
/// Invariants: it should be agnostic to the users of this struct whether a container is stored in `kv` or `store`
pub(crate) struct InnerStore {
    arena: SharedArena,
    store: SlotMap,
    owner: Arc<StoreOwner>,
    kv: KvWrapper,
    load_state: LoadState,
    config: Configure,
    /// FIFO (with a second-chance bit on each wrapper) of containers whose
    /// decoded value is currently cached in `store`. Bounded by
    /// [`MAX_CACHED_CONTAINER_VALUES`]. Entries may be stale (the wrapper was
    /// dropped or materialized into a full state); eviction skips those.
    value_cache_queue: VecDeque<ContainerIdx>,
    /// True once the value cache has evicted at least one wrapper since the
    /// last full KV scan. While this is set, `load_state == AllLoaded` no
    /// longer implies `store` contains every entry in `kv`, so `load_all()`
    /// must re-scan `kv` instead of short-circuiting.
    evicted_since_full_load: bool,
    /// While `Some`, the containers given new (not loaded) state are recorded, so
    /// a rolled back local transaction can remove them again
    /// (`DocState::begin_local_rollback`).
    created_journal: Option<Vec<ContainerIdx>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Lazy,
    RootsLoaded,
    AllLoaded,
}

impl std::fmt::Debug for InnerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InnerStore").finish()
    }
}

/// This impl block contains all the mutation code that may break the invariants of this struct
impl InnerStore {
    pub(super) fn fork_cow(
        &self,
        arena: SharedArena,
        config: &Configure,
        install_resolver: bool,
    ) -> Self {
        let kv = self.kv.clone();
        if install_resolver {
            let resolver_kv = kv.arc_clone();
            arena.set_parent_resolver(Some(move |child_id: ContainerID| {
                let bytes = resolver_kv.get(&child_id.to_bytes())?;
                ContainerWrapper::new_from_bytes(bytes).parent().cloned()
            }));
        }
        let mut fork = Self {
            arena,
            kv,
            config: config.clone(),
            load_state: self.load_state,
            store: self.store.clone(),
            owner: Arc::new(StoreOwner),
            value_cache_queue: VecDeque::new(),
            evicted_since_full_load: self.evicted_since_full_load,
            created_journal: None,
        };
        // Ordinary lazy-value producers always track their decoded values. Reset
        // only this bounded current cache in the child, never enumerate historical
        // slots. Keeping encoded wrappers preserves AllLoaded without a new KV scan.
        // Internal callers that populate values through raw wrapper access must
        // honor the same tracking obligation (explicit full-state walks are separate).
        for &idx in &self.value_cache_queue {
            #[cfg(any(test, feature = "test_utils"))]
            crate::cow_work::container_fork_cache_visit();
            let Some(source) = self.store.get(&Self::slot(idx)) else {
                continue;
            };
            if !source.wrapper.is_evictable_cached_value() {
                continue;
            }
            let private = source.wrapper.fork_cow(config);
            fork.store.insert(
                Self::slot(idx),
                StoreSlot {
                    owner: fork.owner.clone(),
                    wrapper: Arc::new(private),
                },
            );
            #[cfg(any(test, feature = "test_utils"))]
            crate::cow_work::container_fork_cache_reset();
        }
        fork
    }

    #[inline]
    fn slot(idx: ContainerIdx) -> usize {
        idx.to_index() as usize
    }

    #[inline]
    fn get_entry_mut_in<'a>(
        store: &'a mut SlotMap,
        owner: &Arc<StoreOwner>,
        config: &Configure,
        idx: ContainerIdx,
    ) -> Option<&'a mut ContainerWrapper> {
        let entry = store.get_mut(&Self::slot(idx))?;
        debug_assert_eq!(entry.wrapper.kind(), idx.get_type());
        Some(entry.make_mut(owner, config))
    }

    #[inline]
    fn get_entry_mut(&mut self, idx: ContainerIdx) -> Option<&mut ContainerWrapper> {
        Self::get_entry_mut_in(&mut self.store, &self.owner, &self.config, idx)
    }

    #[inline]
    fn contains_idx_in(store: &SlotMap, idx: ContainerIdx) -> bool {
        store
            .get(&Self::slot(idx))
            .is_some_and(|entry| entry.wrapper.kind() == idx.get_type())
    }

    #[inline]
    fn contains_idx(&self, idx: ContainerIdx) -> bool {
        Self::contains_idx_in(&self.store, idx)
    }

    fn insert_entry(
        store: &mut SlotMap,
        owner: &Arc<StoreOwner>,
        idx: ContainerIdx,
        container: ContainerWrapper,
    ) -> Option<StoreSlot> {
        let slot = Self::slot(idx);
        #[cfg(any(test, feature = "test_utils"))]
        if !store.contains_key(&slot) {
            crate::cow_work::container_slot_inserted();
        }
        store.insert(
            slot,
            StoreSlot {
                owner: owner.clone(),
                wrapper: Arc::new(container),
            },
        )
    }

    // Explicit whole-store operations retain the old ascending slot order. Only
    // these enumeration APIs gather/sort references; point access and forks do not.
    fn ordered_entries(store: &SlotMap) -> Vec<(usize, &StoreSlot)> {
        let mut entries: Vec<_> = store.iter().map(|(key, value)| (*key, value)).collect();
        entries.sort_unstable_by_key(|(key, _)| *key);
        #[cfg(any(test, feature = "test_utils"))]
        crate::cow_work::container_slot_enumeration(entries.len());
        entries
    }

    fn ordered_entries_mut(store: &mut SlotMap) -> Vec<(usize, &mut StoreSlot)> {
        let mut entries: Vec<_> = store.iter_mut().map(|(key, value)| (*key, value)).collect();
        entries.sort_unstable_by_key(|(key, _)| *key);
        #[cfg(any(test, feature = "test_utils"))]
        crate::cow_work::container_slot_enumeration(entries.len());
        entries
    }

    pub(super) fn get_or_insert_with(
        &mut self,
        idx: ContainerIdx,
        f: impl FnOnce() -> ContainerWrapper,
    ) -> &mut ContainerWrapper {
        if self.get_entry_mut(idx).is_none() {
            let id = self.arena.get_container_id(idx).unwrap();
            let key = id.to_bytes();
            let container = match self.kv.get(&key) {
                Some(bytes) => ContainerWrapper::new_from_bytes(bytes),
                None => {
                    if let Some(journal) = &mut self.created_journal {
                        journal.push(idx);
                    }
                    f()
                }
            };
            Self::insert_entry(&mut self.store, &self.owner, idx, container);
        }

        self.get_entry_mut(idx).unwrap()
    }

    pub(super) fn ensure_container(
        &mut self,
        idx: ContainerIdx,
        f: impl FnOnce() -> ContainerWrapper,
    ) {
        if self.contains_idx(idx) {
            return;
        }

        let id = self.arena.get_container_id(idx).unwrap();
        let key = id.to_bytes();
        if let Some(v) = self.kv.get(&key) {
            let c = ContainerWrapper::new_from_bytes(v);
            Self::insert_entry(&mut self.store, &self.owner, idx, c);
            return;
        }

        if let Some(journal) = &mut self.created_journal {
            journal.push(idx);
        }
        let c = f();
        Self::insert_entry(&mut self.store, &self.owner, idx, c);
    }

    /// Start recording the containers that get new state. See `created_journal`.
    pub(crate) fn begin_created_journal(&mut self) {
        self.created_journal = Some(Vec::new());
    }

    /// Stop recording and return the containers that got new state since
    /// [`Self::begin_created_journal`].
    pub(crate) fn take_created_journal(&mut self) -> Vec<ContainerIdx> {
        self.created_journal.take().unwrap_or_default()
    }

    /// Drop the state of containers that got new state while a rolled back
    /// local transaction ran. They had no state and no KV entry before it (the
    /// KV store is only written by `flush`), so this restores the store.
    pub(crate) fn remove_created(&mut self, created: &[ContainerIdx]) {
        for &idx in created {
            debug_assert!(self
                .arena
                .get_container_id(idx)
                .is_none_or(|id| self.kv.get(&id.to_bytes()).is_none()));
            self.store.remove(&Self::slot(idx));
        }
    }

    pub(crate) fn get_mut(&mut self, idx: ContainerIdx) -> Option<&mut ContainerWrapper> {
        if self.get_entry_mut(idx).is_none() {
            let id = self.arena.get_container_id(idx).unwrap();
            let key = id.to_bytes();
            if let Some(v) = self.kv.get(&key) {
                let c = ContainerWrapper::new_from_bytes(v);
                Self::insert_entry(&mut self.store, &self.owner, idx, c);
            }
        }

        self.get_entry_mut(idx)
    }

    pub(crate) fn with_container_for_read<R>(
        &mut self,
        idx: ContainerIdx,
        f: impl FnOnce(&mut ContainerWrapper) -> R,
    ) -> Option<R> {
        if let Some(entry) = self.get_entry_mut(idx) {
            let ans = f(entry);
            self.track_value_cache(idx);
            return Some(ans);
        }

        let id = self.arena.get_container_id(idx).unwrap();
        let key = id.to_bytes();
        if let Some(v) = self.kv.get(&key) {
            let mut container = ContainerWrapper::new_from_bytes(v);
            let ans = f(&mut container);
            if container.has_cached_value() {
                Self::insert_entry(&mut self.store, &self.owner, idx, container);
                self.track_value_cache(idx);
            }
            return Some(ans);
        }

        None
    }

    /// Track a wrapper whose read may have populated the decoded-value cache,
    /// and evict the oldest cached values once the cache exceeds
    /// [`MAX_CACHED_CONTAINER_VALUES`].
    ///
    /// Eviction drops the whole wrapper from `store`; the wrapper is a pure
    /// cache over the KV bytes (`ContainerWrapper::is_evictable_cached_value`
    /// requires a flushed lazy wrapper), so the next read re-creates it from
    /// the KV store.
    fn track_value_cache(&mut self, idx: ContainerIdx) {
        let Some(entry) = self.get_entry_mut(idx) else {
            return;
        };
        if !entry.is_evictable_cached_value() {
            return;
        }
        if !entry.is_in_value_cache_queue() {
            entry.set_in_value_cache_queue(true);
            self.value_cache_queue.push_back(idx);
        }
        self.get_entry_mut(idx)
            .unwrap()
            .mark_value_cache_referenced();

        while self.value_cache_queue.len() > MAX_CACHED_CONTAINER_VALUES {
            let Some(victim) = self.value_cache_queue.pop_front() else {
                break;
            };
            enum Action {
                Skip,
                Requeue,
                Evict,
            }
            let action = match self.get_entry_mut(victim) {
                Some(entry) if entry.is_evictable_cached_value() => {
                    if entry.take_value_cache_referenced() {
                        Action::Requeue
                    } else {
                        entry.set_in_value_cache_queue(false);
                        Action::Evict
                    }
                }
                // Stale entry: the wrapper was dropped or materialized into a
                // full state since it was enqueued.
                _ => Action::Skip,
            };
            match action {
                Action::Skip => {}
                Action::Requeue => self.value_cache_queue.push_back(victim),
                Action::Evict => {
                    let slot = Self::slot(victim);
                    self.store.remove(&slot);
                    // `store` may no longer mirror `kv`: a full enumeration has
                    // to re-scan `kv` even in `AllLoaded` mode.
                    self.evicted_since_full_load = true;
                }
            }
        }
    }

    /// Read a container without retaining a wrapper loaded only for this read.
    pub(crate) fn try_with_container_for_ephemeral_read<R>(
        &mut self,
        idx: ContainerIdx,
        f: impl FnOnce(&mut ContainerWrapper) -> R,
    ) -> LoroResult<Option<R>> {
        if let Some(entry) = self.get_entry_mut(idx) {
            return Ok(Some(f(entry)));
        }

        let id = self.arena.get_container_id(idx).unwrap();
        let key = id.to_bytes();
        if let Some(v) = self.kv.get(&key) {
            let mut container = ContainerWrapper::try_new_from_bytes(v)?;
            return Ok(Some(f(&mut container)));
        }

        Ok(None)
    }

    /// Read a container's current value without retaining a wrapper loaded only for this read.
    ///
    /// Mirrors [`Self::try_get_parent_and_value_ephemeral`]: routing the kv branch through
    /// `ContainerWrapper::try_get_value_ephemeral` would clone the bytes into a second temporary
    /// wrapper, so decode the already-temporary wrapper in place instead.
    pub(crate) fn try_get_value_ephemeral(
        &mut self,
        idx: ContainerIdx,
        ctx: ContainerCreationContext<'_>,
    ) -> LoroResult<Option<LoroValue>> {
        if let Some(entry) = self.get_entry_mut(idx) {
            return entry.try_get_value_ephemeral(idx, ctx).map(Some);
        }

        let id = self.arena.get_container_id(idx).unwrap();
        let key = id.to_bytes();
        if let Some(bytes) = self.kv.get(&key) {
            let mut container = ContainerWrapper::try_new_from_bytes(bytes)?;
            return container.try_get_value(idx, ctx).map(Some);
        }

        Ok(None)
    }

    /// Read a container's encoded parent and current value together without retaining a wrapper
    /// loaded only for this read.
    ///
    /// The alive-container walk needs both fields. Reading them through two independent probes can
    /// evict the first SSTable block before the value is requested, and it constructs the same lazy
    /// wrapper twice. Decode an uncached wrapper in place here and drop it after returning the value.
    pub(crate) fn try_get_parent_and_value_ephemeral(
        &mut self,
        idx: ContainerIdx,
        ctx: ContainerCreationContext<'_>,
    ) -> LoroResult<Option<(Option<ContainerID>, LoroValue)>> {
        if let Some(entry) = self.get_entry_mut(idx) {
            let parent = entry.parent().cloned();
            let value = entry.try_get_value_ephemeral(idx, ctx)?;
            return Ok(Some((parent, value)));
        }

        let id = self.arena.get_container_id(idx).unwrap();
        let key = id.to_bytes();
        if let Some(value) = self.kv.get(&key) {
            let mut container = ContainerWrapper::try_new_from_bytes(value)?;
            let parent = container.parent().cloned();
            // This wrapper is already temporary, so decoding into it does not retain state in
            // the document and avoids constructing another temporary wrapper internally.
            let value = container.try_get_value(idx, ctx)?;
            return Ok(Some((parent, value)));
        }

        Ok(None)
    }

    /// Tree counterpart of [`Self::try_get_parent_and_value_ephemeral`]: the encoded parent and
    /// the meta map ids of every node, deleted nodes included.
    pub(crate) fn try_get_parent_and_tree_meta_ids_ephemeral(
        &mut self,
        idx: ContainerIdx,
        ctx: ContainerCreationContext<'_>,
    ) -> LoroResult<Option<(Option<ContainerID>, Vec<ContainerID>)>> {
        if let Some(entry) = self.get_entry_mut(idx) {
            let parent = entry.parent().cloned();
            let ids = entry.try_get_tree_meta_ids_ephemeral(idx, ctx)?;
            return Ok(Some((parent, ids)));
        }

        let id = self.arena.get_container_id(idx).unwrap();
        let key = id.to_bytes();
        if let Some(value) = self.kv.get(&key) {
            let mut container = ContainerWrapper::try_new_from_bytes(value)?;
            let parent = container.parent().cloned();
            container.decode_state(idx, ctx)?;
            let ids = container.try_get_state().unwrap().get_child_containers();
            return Ok(Some((parent, ids)));
        }

        Ok(None)
    }

    /// Read the parent encoded in a container wrapper without retaining a wrapper loaded only
    /// for this probe.
    ///
    /// The outer `Option` distinguishes a missing wrapper from a wrapper whose encoded parent is
    /// `None`.
    pub(crate) fn get_parent_ephemeral(
        &mut self,
        idx: ContainerIdx,
    ) -> LoroResult<Option<Option<ContainerID>>> {
        self.try_with_container_for_ephemeral_read(idx, |container| container.parent().cloned())
    }

    pub(crate) fn has_decoded_state(&mut self, idx: ContainerIdx) -> bool {
        self.get_entry_mut(idx)
            .is_some_and(|entry| entry.try_get_state().is_some())
    }

    pub(crate) fn contains_id(&mut self, id: &ContainerID) -> bool {
        if let Some(idx) = self.arena.id_to_idx(id) {
            if self.contains_idx(idx) {
                return true;
            }
        }

        let key = id.to_bytes();
        self.kv.contains_key(&key)
    }

    /// Tree and MovableList containers, found without decoding any other
    /// container. A kv key starts with the container type byte, with the high
    /// bit set for root ids (see `ContainerID::encode`), so each type is a key range.
    pub(crate) fn tree_and_movable_list_idxs(&mut self) -> Vec<ContainerIdx> {
        let mut keys: Vec<Bytes> = Vec::new();
        for ty in [ContainerType::Tree, ContainerType::MovableList] {
            for root_bit in [0u8, 0b1000_0000] {
                let lo = ty.to_u8() | root_bit;
                keys.extend(
                    self.kv
                        .scan_range_entries(&[lo], &[lo + 1])
                        .into_iter()
                        .map(|(k, _)| k),
                );
            }
        }
        let mut out: FxHashSet<ContainerIdx> = FxHashSet::default();
        self.arena.with_guards(|guards| {
            for k in keys {
                out.insert(guards.register_container(&ContainerID::from_bytes(&k)));
            }
        });
        // Loaded entries can be newer than kv or absent from it.
        for (slot, entry) in Self::ordered_entries(&self.store) {
            let c = &entry.wrapper;
            if matches!(c.kind(), ContainerType::Tree | ContainerType::MovableList) {
                out.insert(ContainerIdx::from_index_and_type(slot as u32, c.kind()));
            }
        }
        out.into_iter().collect()
    }

    pub(crate) fn iter_all_containers_mut(
        &mut self,
    ) -> impl Iterator<Item = (ContainerIdx, &mut ContainerWrapper)> {
        self.load_all();
        let owner = &self.owner;
        let config = &self.config;
        Self::ordered_entries_mut(&mut self.store)
            .into_iter()
            .map(move |(slot, entry)| {
                let container = entry.make_mut(owner, config);
                (
                    ContainerIdx::from_index_and_type(slot as u32, container.kind()),
                    container,
                )
            })
    }

    pub(crate) fn iter_all_container_ids(&mut self) -> impl Iterator<Item = ContainerID> + '_ {
        // PERF: we don't need to load all the containers here
        self.load_all();
        Self::ordered_entries(&self.store)
            .into_iter()
            .map(|(slot, entry)| {
                let idx = ContainerIdx::from_index_and_type(slot as u32, entry.wrapper.kind());
                self.arena.get_container_id(idx).unwrap()
            })
    }

    pub(crate) fn encode(&mut self) -> Bytes {
        self.flush();
        self.kv.export()
    }

    pub(crate) fn flush(&mut self) {
        let deleted = self.config.deleted_root_containers.lock();
        let mut updates = Vec::new();
        let mut deleted_roots = Vec::new();

        for (slot, entry) in Self::ordered_entries_mut(&mut self.store) {
            let c = entry.make_mut(&self.owner, &self.config);
            let idx = ContainerIdx::from_index_and_type(slot as u32, c.kind());
            let cid = self.arena.get_container_id(idx).unwrap();
            if cid.is_root() && deleted.contains(&cid) && c.is_deleted_root_value_cleared() {
                deleted_roots.push(cid.to_bytes());
                c.set_flushed(true);
                continue;
            }

            if c.is_flushed() {
                continue;
            }

            let cid: Bytes = cid.to_bytes().into();
            let value = c.encode();
            c.set_flushed(true);
            updates.push((cid, value));
        }

        drop(deleted);
        for cid in deleted_roots {
            self.kv.remove(&cid);
        }
        self.kv.set_all(updates);
    }

    pub(crate) fn get_kv_clone(&self) -> KvWrapper {
        self.kv.clone()
    }

    pub(crate) fn decode(
        &mut self,
        bytes: bytes::Bytes,
    ) -> Result<Option<Frontiers>, loro_common::LoroError> {
        assert!(self.kv.is_empty());
        let mut fr = None;
        self.kv
            .import(bytes)
            .map_err(|e| loro_common::LoroError::DecodeError(e.into_boxed_str()))?;
        if let Some(f) = self.kv.remove(FRONTIERS_KEY) {
            fr = Some(Frontiers::decode(&f)?);
        }

        let kv = self.kv.arc_clone();
        self.arena
            .set_parent_resolver(Some(move |child_id: ContainerID| {
                let k = child_id.to_bytes();
                let v = kv.get(&k)?;
                let c = ContainerWrapper::new_from_bytes(v);
                c.parent().cloned()
            }));

        self.store.clear();
        self.value_cache_queue.clear();
        self.evicted_since_full_load = false;
        self.load_state = LoadState::Lazy;
        Ok(fr)
    }

    pub(crate) fn decode_twice(
        &mut self,
        bytes_a: bytes::Bytes,
        bytes_b: bytes::Bytes,
    ) -> Result<(), loro_common::LoroError> {
        assert!(self.kv.is_empty());
        // TODO: add assert that all containers in the store should be empty right now
        self.kv
            .import(bytes_a)
            .map_err(|e| loro_common::LoroError::DecodeError(e.into_boxed_str()))?;
        self.kv
            .import(bytes_b)
            .map_err(|e| loro_common::LoroError::DecodeError(e.into_boxed_str()))?;
        self.kv.remove(FRONTIERS_KEY);
        let entries = self.kv.scan_all_entries();
        let store = &mut self.store;
        let arena = &self.arena;
        arena.with_guards(|guards| {
            for (k, v) in entries {
                let cid = ContainerID::from_bytes(&k);
                let c = ContainerWrapper::new_from_bytes(v);
                let parent = c.parent();
                let idx = guards.register_container(&cid);
                let p = parent.as_ref().map(|p| guards.register_container(p));
                guards.set_parent(idx, p);
                if Self::insert_entry(store, &self.owner, idx, c).is_some() {}
            }
        });

        // Entries were rebuilt from the KV store; drop any queue entries that
        // refer to replaced wrappers so the queue only names wrappers that are
        // actually enqueued.
        self.value_cache_queue.clear();
        for (_, entry) in Self::ordered_entries_mut(&mut self.store) {
            entry
                .make_mut(&self.owner, &self.config)
                .set_in_value_cache_queue(false);
        }

        self.evicted_since_full_load = false;
        self.load_state = LoadState::AllLoaded;
        Ok(())
    }

    pub fn load_all(&mut self) {
        if self.load_state == LoadState::AllLoaded && !self.evicted_since_full_load {
            return;
        }

        let entries = self.kv.scan_all_entries();
        let store = &mut self.store;
        let arena = &self.arena;
        arena.with_guards(|guards| {
            for (k, v) in entries {
                let cid = ContainerID::from_bytes(&k);
                let idx = guards.register_container(&cid);
                if Self::contains_idx_in(store, idx) {
                    // the container is already loaded
                    // the content in `store` is guaranteed to be newer than the content in `kv`
                    continue;
                }

                let container = ContainerWrapper::new_from_bytes(v);
                Self::insert_entry(store, &self.owner, idx, container);
            }
        });

        self.evicted_since_full_load = false;
        self.load_state = LoadState::AllLoaded;
    }

    pub fn load_roots(&mut self) {
        if self.load_state != LoadState::Lazy {
            return;
        }

        let arena = &self.arena;
        let keys = self.kv.scan_all_keys();
        arena.with_guards(|guards| {
            for k in keys {
                let cid = ContainerID::from_bytes(&k);
                if cid.is_root() {
                    guards.register_container(&cid);
                }
            }
        });
        self.load_state = LoadState::RootsLoaded;
    }

    pub(crate) fn can_import_snapshot(&self) -> bool {
        if !self.kv.is_empty() {
            return false;
        }

        Self::ordered_entries(&self.store)
            .into_iter()
            .all(|(_, c)| c.wrapper.is_state_empty())
    }

    #[cfg(test)]
    pub(super) fn has_cached_value_for_test(&mut self, idx: ContainerIdx) -> bool {
        self.get_entry_mut(idx)
            .is_some_and(|entry| entry.has_cached_value_for_test())
    }

    /// Number of wrappers currently holding an evictable cached decoded value.
    /// Must stay bounded by [`MAX_CACHED_CONTAINER_VALUES`] no matter how many
    /// containers have been read (loro-dev/loro#1092).
    #[cfg(test)]
    pub(super) fn cached_value_count_for_test(&self) -> usize {
        self.store
            .values()
            .filter(|entry| entry.wrapper.is_evictable_cached_value())
            .count()
    }

    #[cfg(test)]
    pub(super) fn has_materialized_map_value_for_test(&mut self, idx: ContainerIdx) -> bool {
        self.get_entry_mut(idx)
            .is_some_and(|entry| entry.has_materialized_map_value_for_test())
    }
}

impl InnerStore {
    pub(crate) fn new(arena: SharedArena, config: Configure) -> Self {
        Self {
            arena,
            store: SlotMap::default(),
            owner: Arc::new(StoreOwner),
            kv: KvWrapper::new_mem(),
            load_state: LoadState::AllLoaded,
            config,
            value_cache_queue: VecDeque::new(),
            evicted_since_full_load: false,
            created_journal: None,
        }
    }

    pub(crate) fn fork(&mut self, arena: SharedArena, config: &Configure) -> InnerStore {
        // PERF: we can try to reuse
        let bytes = self.encode();
        let mut new_store = Self::new(arena, config.clone());
        new_store.decode(bytes).unwrap();
        new_store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro_common::{ContainerType, ID};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    fn text_owner_facts(doc: &crate::LoroDoc) -> (usize, bool) {
        let mut state = doc.app_state().lock();
        let idx = state
            .store
            .arena
            .register_container(&ContainerID::new_root("text", ContainerType::Text));
        let wrapper = state.store.store.get_mut(idx).unwrap();
        let crate::state::State::RichtextState(text) = wrapper.try_get_state().unwrap() else {
            panic!("expected live text state")
        };
        (
            text.get_version_id(),
            Arc::ptr_eq(text.config_for_test(), doc.config().text_style_config()),
        )
    }

    #[test]
    fn cow_slot_access_preserves_source_version_and_rebinds_after_source_drop() {
        let source = crate::LoroDoc::new_auto_commit();
        source
            .get_text("text")
            .insert(0, "A界🙂Z", crate::cursor::PosType::Unicode)
            .unwrap();
        source.commit_then_renew();
        let (before, bound) = text_owner_facts(&source);
        assert!(before > 0 && bound);
        let child = source.fork_cow().unwrap();
        assert_eq!(text_owner_facts(&source), (before, true));
        assert!(!Arc::ptr_eq(
            source.config().text_style_config(),
            child.config().text_style_config()
        ));
        drop(source);
        assert_eq!(text_owner_facts(&child), (0, true));
        assert_eq!(child.get_text("text").to_string(), "A界🙂Z");
        child
            .get_text("text")
            .insert(1, "!", crate::cursor::PosType::Unicode)
            .unwrap();
        child.commit_then_renew();
        assert!(text_owner_facts(&child).0 > 0);
    }

    fn lazy_map_value(store: &mut InnerStore, idx: ContainerIdx) -> LoroResult<LoroValue> {
        let config = store.config.clone();
        store
            .with_container_for_read(idx, |entry| {
                entry.try_get_value(
                    idx,
                    ContainerCreationContext {
                        configure: &config,
                        peer: 7,
                    },
                )
            })
            .unwrap()
    }

    #[test]
    fn cow_same_owner_queue_survives_detach_and_evicts_without_affecting_child() {
        let doc = crate::LoroDoc::new_auto_commit();
        let n = MAX_CACHED_CONTAINER_VALUES * 2;
        for i in 0..n {
            doc.get_map(format!("map{i}"))
                .insert("value", i as i64)
                .unwrap();
        }
        doc.commit_then_renew();
        let bytes = doc.app_state().lock().store.encode();
        let arena = SharedArena::new();
        let config = Configure::default();
        let mut source = InnerStore::new(arena.clone(), config.clone());
        source.decode(bytes).unwrap();
        let indices: Vec<_> = (0..n)
            .map(|i| {
                arena.register_container(&ContainerID::new_root(
                    &format!("map{i}"),
                    ContainerType::Map,
                ))
            })
            .collect();
        let first = lazy_map_value(&mut source, indices[0]).unwrap();
        assert_eq!(source.value_cache_queue.len(), 1);
        let mut child = source.fork_cow(arena.fork_cow(), &config.fork(), false);
        assert_eq!(lazy_map_value(&mut source, indices[0]).unwrap(), first);
        assert_eq!(source.value_cache_queue.len(), 1);
        assert!(source.has_cached_value_for_test(indices[0]));
        assert!(child.value_cache_queue.is_empty());
        assert_eq!(lazy_map_value(&mut child, indices[0]).unwrap(), first);
        assert_eq!(child.value_cache_queue.len(), 1);
        for idx in &indices[1..] {
            assert!(lazy_map_value(&mut source, *idx).is_ok());
        }
        assert!(source.cached_value_count_for_test() <= MAX_CACHED_CONTAINER_VALUES);
        assert!(!source.has_cached_value_for_test(indices[0]));
        assert_eq!(lazy_map_value(&mut source, indices[0]).unwrap(), first);
        assert!(source.value_cache_queue.len() <= MAX_CACHED_CONTAINER_VALUES);
        assert_eq!(child.value_cache_queue.len(), 1);
        assert!(child.has_cached_value_for_test(indices[0]));
        assert_eq!(lazy_map_value(&mut child, indices[0]).unwrap(), first);
        let config = source.config.clone();
        let mut entries = source.iter_all_containers_mut();
        let (a, left) = entries.next().unwrap();
        let (b, right) = entries.next().unwrap();
        assert_ne!(a, b);
        assert!(!std::ptr::eq(left, right));
        // Keep both mutable references alive at once; each decoder must retain
        // an independent wrapper, not a reused iterator scratch location.
        let ctx = ContainerCreationContext {
            configure: &config,
            peer: 7,
        };
        let left_value = left.try_get_value(a, ctx).unwrap();
        let right_value = right.try_get_value(b, ctx).unwrap();
        assert_ne!(left_value, right_value);
        let mut order = vec![a.to_index(), b.to_index()];
        order.extend(entries.map(|(idx, _)| idx.to_index()));
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "explicit mutable enumeration must preserve ordinal order"
        );
    }

    #[test]
    fn cow_repeated_fork_disjoint_lazy_reads_keep_cache_bounded_and_tracked() {
        let doc = crate::LoroDoc::new_auto_commit();
        let n = MAX_CACHED_CONTAINER_VALUES * 3;
        for i in 0..n {
            doc.get_map(format!("map{i}"))
                .insert("value", i as i64)
                .unwrap();
        }
        doc.commit_then_renew();
        let bytes = doc.app_state().lock().store.encode();
        let arena = SharedArena::new();
        let mut current = InnerStore::new(arena.clone(), Configure::default());
        current.decode(bytes).unwrap();
        let indices: Vec<_> = (0..n)
            .map(|i| {
                arena.register_container(&ContainerID::new_root(
                    &format!("map{i}"),
                    ContainerType::Map,
                ))
            })
            .collect();
        let mut held = Vec::new();
        for (i, idx) in indices.iter().enumerate() {
            let next = current.fork_cow(current.arena.fork_cow(), &current.config.fork(), false);
            assert_eq!(next.load_state, current.load_state);
            assert_eq!(
                next.evicted_since_full_load,
                current.evicted_since_full_load
            );
            held.push(current);
            current = next;
            let value = lazy_map_value(&mut current, *idx).unwrap();
            assert_eq!(
                value.as_map().unwrap().get("value"),
                Some(&LoroValue::I64(i as i64))
            );
            let cached = current.cached_value_count_for_test();
            assert!(
                cached <= MAX_CACHED_CONTAINER_VALUES,
                "fork {i}: {cached} physically cached values"
            );
            assert!(
                cached <= current.value_cache_queue.len(),
                "fork {i}: cached value escaped queue ownership"
            );
        }
        for (i, owner) in held.iter_mut().enumerate().skip(1) {
            let value = lazy_map_value(owner, indices[i - 1]).unwrap();
            assert_eq!(
                value.as_map().unwrap().get("value"),
                Some(&LoroValue::I64((i - 1) as i64))
            );
        }
    }

    #[test]
    fn cow_cold_decode_failure_does_not_publish_value_or_queue_entry() {
        let arena = SharedArena::new();
        let id = ContainerID::new_root("broken", ContainerType::Map);
        let idx = arena.register_container(&id);
        let mut source = InnerStore::new(arena.clone(), Configure::default());
        source.kv.set_all(vec![(
            Bytes::from(id.to_bytes()),
            encoded_container_header(ContainerType::Map, None),
        )]);
        let expected = source.kv.export();
        let mut child = source.fork_cow(arena.fork_cow(), &Configure::default(), false);
        drop(source);
        assert!(lazy_map_value(&mut child, idx).is_err());
        assert!(child.value_cache_queue.is_empty());
        assert_eq!(child.cached_value_count_for_test(), 0);
        assert_eq!(child.kv.export(), expected);
    }

    fn encoded_container_header(kind: ContainerType, parent: Option<ContainerID>) -> Bytes {
        let mut output = Vec::new();
        output.push(kind.to_u8());
        leb128::write::unsigned(&mut output, 2).unwrap();
        postcard::to_io(&parent, &mut output).unwrap();
        output.into()
    }

    fn mergeable_child_entry() -> (ContainerID, Bytes) {
        let parent_id = ContainerID::new_normal(ID::new(1, 0), ContainerType::Map);
        let child_id = ContainerID::new_mergeable(&parent_id, "field", ContainerType::Text);
        let value = encoded_container_header(ContainerType::Text, Some(parent_id));
        (child_id, value)
    }

    fn install_parent_resolver_guard(
        arena: &SharedArena,
        loading: Arc<AtomicBool>,
    ) -> Arc<AtomicBool> {
        let resolver_called = Arc::new(AtomicBool::new(false));
        let called = resolver_called.clone();
        arena.set_parent_resolver(Some(move |_child_id: ContainerID| {
            assert!(
                !loading.load(Ordering::SeqCst),
                "lazy parent resolver must not run while loading from KV"
            );
            called.store(true, Ordering::SeqCst);
            None
        }));
        resolver_called
    }

    fn kv_bytes_with_entry(cid: ContainerID, value: Bytes) -> Bytes {
        let kv = KvWrapper::new_mem();
        kv.set_all(vec![(Bytes::from(cid.to_bytes()), value)]);
        kv.export()
    }

    #[test]
    fn load_all_does_not_resolve_parent_while_loading_from_kv() {
        let arena = SharedArena::new();
        let mut store = InnerStore::new(arena.clone(), Configure::default());
        let loading = Arc::new(AtomicBool::new(false));
        let resolver_called = install_parent_resolver_guard(&arena, loading.clone());
        let (child_id, value) = mergeable_child_entry();
        let child_id_for_depth = child_id.clone();
        store.load_state = LoadState::Lazy;
        store
            .kv
            .set_all(vec![(Bytes::from(child_id.to_bytes()), value)]);

        loading.store(true, Ordering::SeqCst);
        store.load_all();
        loading.store(false, Ordering::SeqCst);
        assert!(!resolver_called.load(Ordering::SeqCst));

        let child_idx = arena.id_to_idx(&child_id_for_depth).unwrap();
        let _ = arena.get_depth(child_idx);
        assert!(
            resolver_called.load(Ordering::SeqCst),
            "lazy parent resolver should still work after load_all returns"
        );
    }

    #[test]
    fn load_roots_does_not_resolve_parent_while_loading_from_kv() {
        let arena = SharedArena::new();
        let mut store = InnerStore::new(arena.clone(), Configure::default());
        let loading = Arc::new(AtomicBool::new(false));
        let resolver_called = install_parent_resolver_guard(&arena, loading.clone());
        let (child_id, value) = mergeable_child_entry();
        let child_id_for_depth = child_id.clone();
        store.load_state = LoadState::Lazy;
        store
            .kv
            .set_all(vec![(Bytes::from(child_id.to_bytes()), value)]);

        loading.store(true, Ordering::SeqCst);
        store.load_roots();
        loading.store(false, Ordering::SeqCst);
        assert!(!resolver_called.load(Ordering::SeqCst));

        let child_idx = arena.id_to_idx(&child_id_for_depth).unwrap();
        let _ = arena.get_depth(child_idx);
        assert!(
            resolver_called.load(Ordering::SeqCst),
            "lazy parent resolver should still work after load_roots returns"
        );
    }

    #[test]
    fn decode_twice_does_not_resolve_parent_while_loading_from_kv() {
        let arena = SharedArena::new();
        let mut store = InnerStore::new(arena.clone(), Configure::default());
        let loading = Arc::new(AtomicBool::new(false));
        let resolver_called = install_parent_resolver_guard(&arena, loading.clone());
        let (child_id, value) = mergeable_child_entry();
        let child_id_for_depth = child_id.clone();
        let bytes = kv_bytes_with_entry(child_id, value);

        loading.store(true, Ordering::SeqCst);
        store.decode_twice(bytes, Bytes::new()).unwrap();
        loading.store(false, Ordering::SeqCst);
        assert!(!resolver_called.load(Ordering::SeqCst));

        let child_idx = arena.id_to_idx(&child_id_for_depth).unwrap();
        let _ = arena.get_depth(child_idx);
        assert!(
            resolver_called.load(Ordering::SeqCst),
            "lazy parent resolver should still work after decode_twice returns"
        );
    }
}
