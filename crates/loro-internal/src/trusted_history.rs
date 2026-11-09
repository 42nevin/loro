//! Experimental trusted local current-state/history backing. Not a remote import
//! format. Callers atomically bind state, complete key directory and immutable
//! backing references to an already accepted root, and verify bytes in `read`.
//! Use only isolated candidates inside `guard_trusted_history`; discard a
//! candidate after any storage failure. No default feature enables this module.
use crate::{
    kv_store::{KvStore, MemKvStore},
    sync::Mutex,
    LoroDoc,
};
use bytes::Bytes;
use loro_kv_store::mem_store::MemKvConfig;
use serde::{Deserialize, Serialize};
use std::{
    collections::{btree_map, BTreeMap, BTreeSet},
    ops::Bound,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

/// The source must retain immutable, root-qualified bytes for the entire lifetime
/// of every branch. It must verify its own object commitments before returning.
pub trait TrustedHistorySource: std::fmt::Debug + Send + Sync {
    fn read(&self, key: &[u8]) -> Result<Vec<u8>, String>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub key: Vec<u8>,
    pub bytes_len: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HistoryValue {
    /// Exactly this key in the original attached source. Never means a private
    /// unpersisted overlay value. Preserve that immutable object reference.
    Retained { bytes_len: usize },
    /// New/modified value requiring persistence before root publication.
    Written(Vec<u8>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedHistoryCapture {
    pub state: Vec<u8>,
    pub history: Vec<(Vec<u8>, HistoryValue)>,
    /// True only for the initial unbacked full history capture. Applications can
    /// hard-refuse this in an ordinary backed edit publication path.
    pub bootstrap_full_export: bool,
}
#[derive(Debug, Clone, thiserror::Error)]
pub enum TrustedHistoryError {
    #[error("trusted history storage failure; discard candidate: {0}")]
    Storage(String),
    #[error("trusted history operation refused: {0}")]
    Invalid(String),
}
#[derive(Debug)]
struct StorageFault(String);
/// Catches only this module's storage fault. Other panics propagate unchanged.
/// This is a containment boundary, not rollback: never keep using the candidate.
/// All edits, imports, checks and capture on a backed candidate must be inside
/// this guard; publish only after the entire guarded operation succeeds.
pub fn guard_trusted_history<T>(operation: impl FnOnce() -> T) -> Result<T, TrustedHistoryError> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(error) => match error.downcast::<StorageFault>() {
            Ok(fault) => Err(TrustedHistoryError::Storage(fault.0)),
            Err(other) => std::panic::resume_unwind(other),
        },
    }
}
#[derive(Debug)]
struct Backing {
    source: Arc<dyn TrustedHistorySource>,
    entries: Arc<BTreeMap<Vec<u8>, usize>>,
    overlay: BTreeMap<Vec<u8>, Option<Bytes>>,
    failure: Mutex<Option<String>>,
    failed: Arc<crate::sync::AtomicBool>,
}
// Borrowed range cursors share one unconsumed interval in each map. Looking at
// either end via a cloned cursor does not consume it or clone any keys. Equal
// keys are consumed from both maps together, with the overlay winning, so
// arbitrary interleaving of next/next_back cannot return a key twice.
struct VisibleRange<'a> {
    entries: btree_map::Range<'a, Vec<u8>, usize>,
    overlay: btree_map::Range<'a, Vec<u8>, Option<Bytes>>,
}
impl<'a> VisibleRange<'a> {
    fn advance(&mut self, back: bool) -> Option<&'a Vec<u8>> {
        loop {
            let base = if back {
                self.entries.clone().next_back()
            } else {
                self.entries.clone().next()
            };
            let changed = if back {
                self.overlay.clone().next_back()
            } else {
                self.overlay.clone().next()
            };
            let (take_base, take_overlay) = match (base, changed) {
                (None, None) => return None,
                (Some(_), None) => (true, false),
                (None, Some(_)) => (false, true),
                (Some((a, _)), Some((b, _))) => match a.cmp(b) {
                    std::cmp::Ordering::Equal => (true, true),
                    std::cmp::Ordering::Less => (!back, back),
                    std::cmp::Ordering::Greater => (back, !back),
                },
            };
            let base = if take_base {
                if back {
                    self.entries.next_back()
                } else {
                    self.entries.next()
                }
            } else {
                None
            };
            if take_overlay {
                let (key, value) = if back {
                    self.overlay.next_back()
                } else {
                    self.overlay.next()
                }
                .unwrap();
                if value.is_some() {
                    return Some(key);
                }
                // A tombstone hides the old value, including when both streams
                // have this key. Continue without reading either payload.
            } else {
                return Some(base.unwrap().0);
            }
        }
    }
}
impl<'a> Iterator for VisibleRange<'a> {
    type Item = &'a Vec<u8>;
    fn next(&mut self) -> Option<Self::Item> {
        self.advance(false)
    }
}
impl DoubleEndedIterator for VisibleRange<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.advance(true)
    }
}
impl std::iter::FusedIterator for VisibleRange<'_> {}

// The previous filter-based scan returned an empty iterator for all reversed
// or excluded-equal bounds. BTreeMap::range would panic for some of those, so
// preserve the existing KvStore behavior before constructing range cursors.
fn empty_range(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Unbounded, _) | (_, Bound::Unbounded) => false,
        (Bound::Included(a), Bound::Included(b)) => a > b,
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            a >= b
        }
    }
}
impl Backing {
    fn fail(&self, error: String) -> ! {
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        *self.failure.lock() = Some(error.clone());
        std::panic::panic_any(StorageFault(error))
    }
    fn check(&self) {
        let error = self.failure.lock().clone();
        if let Some(error) = error {
            self.fail(error);
        }
    }
    fn keys(&self) -> Vec<Vec<u8>> {
        let mut keys: BTreeSet<_> = self.entries.keys().cloned().collect();
        for (key, value) in &self.overlay {
            if value.is_some() {
                keys.insert(key.clone());
            } else {
                keys.remove(key);
            }
        }
        keys.into_iter().collect()
    }
}
impl KvStore for Backing {
    fn get(&self, key: &[u8]) -> Option<Bytes> {
        self.check();
        if let Some(value) = self.overlay.get(key) {
            return value.clone();
        }
        let len = *self.entries.get(key)?;
        let bytes = self
            .source
            .read(key)
            .unwrap_or_else(|error| self.fail(error));
        if bytes.len() != len {
            self.fail("history value length differs from trusted directory".into());
        }
        Some(bytes.into())
    }
    fn set(&mut self, key: &[u8], value: Bytes) {
        self.check();
        self.overlay.insert(key.to_vec(), Some(value));
    }
    fn compare_and_swap(&mut self, key: &[u8], old: Option<Bytes>, new: Bytes) -> bool {
        if self.get(key) != old {
            return false;
        }
        self.set(key, new);
        true
    }
    fn remove(&mut self, key: &[u8]) -> Option<Bytes> {
        let old = self.get(key);
        self.overlay.insert(key.to_vec(), None);
        old
    }
    fn contains_key(&self, key: &[u8]) -> bool {
        self.check();
        self.overlay
            .get(key)
            .map_or_else(|| self.entries.contains_key(key), Option::is_some)
    }
    fn scan(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Box<dyn DoubleEndedIterator<Item = (Bytes, Bytes)> + '_> {
        self.check();
        if empty_range(start, end) {
            return Box::new(std::iter::empty());
        }
        let keys = VisibleRange {
            entries: self.entries.range::<[u8], _>((start, end)),
            overlay: self.overlay.range::<[u8], _>((start, end)),
        };
        Box::new(keys.map(move |key| {
            let value = self.get(key).expect("trusted indexed key");
            (Bytes::copy_from_slice(key), value)
        }))
    }

    fn len(&self) -> usize {
        self.keys().len()
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn size(&self) -> usize {
        self.keys()
            .iter()
            .map(|k| {
                self.overlay
                    .get(k)
                    .and_then(|v| v.as_ref())
                    .map_or_else(|| self.entries[k], Bytes::len)
            })
            .sum()
    }
    fn export_all(&mut self) -> Bytes {
        self.check();
        let mut kv = MemKvStore::new(MemKvConfig::default());
        for key in self.keys() {
            kv.set(&key, self.get(&key).unwrap());
        }
        kv.export_all()
    }
    fn import_all(&mut self, _bytes: Bytes) -> Result<(), String> {
        Err("trusted history backing is not an external snapshot import target".into())
    }
    fn clone_store(&self) -> Arc<Mutex<dyn KvStore>> {
        self.check();
        Arc::new(Mutex::new(Self {
            source: self.source.clone(),
            entries: self.entries.clone(),
            overlay: self.overlay.clone(),
            failure: Mutex::new(None),
            failed: Arc::new(crate::sync::AtomicBool::new(false)),
        }))
    }
    fn trusted_history_capture(&self) -> Option<Vec<(Vec<u8>, HistoryValue)>> {
        self.check();
        Some(
            self.keys()
                .into_iter()
                .map(|key| {
                    let value = match self.overlay.get(&key) {
                        Some(Some(bytes)) => HistoryValue::Written(bytes.to_vec()),
                        None => HistoryValue::Retained {
                            bytes_len: self.entries[&key],
                        },
                        _ => unreachable!(),
                    };
                    (key, value)
                })
                .collect(),
        )
    }
    fn trusted_history_failure(&self) -> Option<String> {
        self.failure.lock().clone()
    }
    fn trusted_history_failure_marker(&self) -> Option<Arc<crate::sync::AtomicBool>> {
        Some(self.failed.clone())
    }
}
fn invalid(error: impl std::fmt::Display) -> TrustedHistoryError {
    TrustedHistoryError::Invalid(error.to_string())
}
impl LoroDoc {
    /// Restore a trusted accepted local cut, not arbitrary external bytes.
    /// The current state is complete; historical bodies are loaded on demand.
    /// The key directory is eager. No shallow trimming is permitted.
    pub fn open_trusted_history(
        state: &[u8],
        entries: Vec<HistoryEntry>,
        source: Arc<dyn TrustedHistorySource>,
    ) -> Result<Self, TrustedHistoryError> {
        guard_trusted_history(|| {
            let count = entries.len();
            let entries: BTreeMap<_, _> =
                entries.into_iter().map(|e| (e.key, e.bytes_len)).collect();
            if entries.len() != count
                || !entries.contains_key(b"vv".as_slice())
                || !entries.contains_key(b"fr".as_slice())
            {
                return Err(invalid("duplicate/missing history metadata keys"));
            }
            let doc = Self::new();
            let backing = Backing {
                source,
                entries: Arc::new(entries),
                overlay: BTreeMap::new(),
                failure: Mutex::new(None),
                failed: doc.trusted_history_failed.clone(),
            };
            {
                let mut oplog = doc.oplog().lock();
                let mut current = doc.app_state().lock();
                current.check_before_decode_snapshot().map_err(invalid)?;
                oplog
                    .attach_trusted_history_backing(Arc::new(Mutex::new(backing)))
                    .map_err(invalid)?;
                if oplog.is_shallow() {
                    return Err(invalid(
                        "trusted history requires complete non-shallow history",
                    ));
                }
                current
                    .store
                    .decode(Bytes::copy_from_slice(state))
                    .map_err(invalid)?;
                current
                    .init_with_states_and_version(
                        oplog.frontiers().clone(),
                        &oplog,
                        vec![],
                        false,
                        Default::default(),
                    )
                    .map_err(invalid)?;
            }
            doc.start_auto_commit();
            Ok(doc)
        })?
    }
    /// Capture current state plus exact history key changes. A backed document
    /// flushes only dirty ChangeBlocks and emits Retained for untouched backing;
    /// it never exports all historical payloads here. Full key metadata is eager.
    pub fn capture_trusted_history(&self) -> Result<TrustedHistoryCapture, TrustedHistoryError> {
        if self
            .trusted_history_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(TrustedHistoryError::Storage(
                "discard failed trusted history candidate".into(),
            ));
        }
        guard_trusted_history(|| {
            self.with_barrier(|| {
                let oplog = self.oplog().lock();
                let mut state = self.app_state().lock();
                if self.is_detached()
                    || oplog.is_shallow()
                    || state.store.shallow_root_store().is_some()
                {
                    return Err(invalid(
                        "trusted capture requires attached non-shallow state",
                    ));
                }
                let (history, bootstrap_full_export) = oplog
                    .change_store()
                    .capture_trusted_history(oplog.vv(), oplog.frontiers())
                    .map_err(invalid)?;
                Ok(TrustedHistoryCapture {
                    state: state.store.encode().to_vec(),
                    history,
                    bootstrap_full_export,
                })
            })
        })?
    }
}

#[cfg(all(test, not(loom)))]
mod range_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Source {
        values: BTreeMap<Vec<u8>, Vec<u8>>,
        reads: AtomicUsize,
    }
    impl TrustedHistorySource for Source {
        fn read(&self, key: &[u8]) -> Result<Vec<u8>, String> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.values.get(key).cloned().ok_or("missing key".into())
        }
    }
    fn backing(values: BTreeMap<Vec<u8>, Vec<u8>>) -> (Backing, Arc<Source>) {
        let entries = values.iter().map(|(k, v)| (k.clone(), v.len())).collect();
        let source = Arc::new(Source {
            values,
            reads: AtomicUsize::new(0),
        });
        (
            Backing {
                source: source.clone(),
                entries: Arc::new(entries),
                overlay: BTreeMap::new(),
                failure: Mutex::new(None),
                failed: Arc::new(crate::sync::AtomicBool::new(false)),
            },
            source,
        )
    }
    fn admitted(key: &[u8], start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
        (match start {
            Bound::Included(s) => key >= s,
            Bound::Excluded(s) => key > s,
            Bound::Unbounded => true,
        }) && (match end {
            Bound::Included(e) => key <= e,
            Bound::Excluded(e) => key < e,
            Bound::Unbounded => true,
        })
    }
    #[test]
    fn scan_matches_materialized_oracle_for_bounds_overlays_and_mixed_ends() {
        // Every key is absent, retained, overwritten, or deleted, including
        // tombstones for absent keys. Compare against ordinary map updates,
        // not a second implementation of the streaming merge.
        let keys: Vec<Vec<u8>> = (0..4).map(|n| vec![n]).collect();
        let mut bounds = vec![Bound::Unbounded];
        for key in &keys {
            bounds.push(Bound::Included(key.as_slice()));
            bounds.push(Bound::Excluded(key.as_slice()));
        }
        for base_mask in 0..16 {
            for overlay_states in 0..81 {
                let original: BTreeMap<_, _> = keys
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| base_mask & (1 << i) != 0)
                    .map(|(_, k)| (k.clone(), vec![10 + k[0]]))
                    .collect();
                let (mut store, _) = backing(original.clone());
                let mut oracle = original;
                let mut states = overlay_states;
                for key in &keys {
                    match states % 3 {
                        1 => {
                            let value = vec![20 + key[0]];
                            store.set(key, value.clone().into());
                            oracle.insert(key.clone(), value);
                        }
                        2 => {
                            store.remove(key);
                            oracle.remove(key);
                        }
                        _ => {}
                    }
                    states /= 3;
                }
                for &start in &bounds {
                    for &end in &bounds {
                        let expected: Vec<_> = oracle
                            .iter()
                            .filter(|(k, _)| admitted(k, start, end))
                            .map(|(k, v)| (Bytes::copy_from_slice(k), Bytes::copy_from_slice(v)))
                            .collect();
                        // All possible front/back consumption schedules, including
                        // continued consumption after ranges have crossed/exhausted.
                        for mask in 0..(1 << expected.len()) {
                            let mut remaining: VecDeque<_> = expected.clone().into();
                            let mut actual = store.scan(start, end);
                            for i in 0..expected.len() {
                                if mask & (1 << i) == 0 {
                                    assert_eq!(actual.next(), remaining.pop_front());
                                } else {
                                    assert_eq!(actual.next_back(), remaining.pop_back());
                                }
                            }
                            assert_eq!(actual.next(), None);
                            assert_eq!(actual.next_back(), None);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn scan_loads_only_consumed_values_and_overlay_shadows_source() {
        let (mut store, source) = backing(
            [
                (b"a".to_vec(), vec![1]),
                (b"c".to_vec(), vec![3]),
                (b"e".to_vec(), vec![5]),
            ]
            .into(),
        );
        store.set(b"c", Bytes::from_static(&[9]));
        store.remove(b"e");
        store.set(b"b", Bytes::from_static(&[2]));
        source.reads.store(0, Ordering::Relaxed);
        let mut iter = store.scan(Bound::Unbounded, Bound::Unbounded);
        assert_eq!(source.reads.load(Ordering::Relaxed), 0);
        assert_eq!(
            iter.next_back(),
            Some((Bytes::from_static(b"c"), Bytes::from_static(&[9])))
        );
        assert_eq!(
            iter.next_back(),
            Some((Bytes::from_static(b"b"), Bytes::from_static(&[2])))
        );
        assert_eq!(source.reads.load(Ordering::Relaxed), 0);
        assert_eq!(
            iter.next(),
            Some((Bytes::from_static(b"a"), Bytes::from_static(&[1])))
        );
        assert_eq!(source.reads.load(Ordering::Relaxed), 1);
        assert_eq!(iter.next_back(), None);
        drop(iter);
        assert_eq!(
            store
                .scan(Bound::Excluded(b"z"), Bound::Included(b"a"))
                .next(),
            None
        );
        assert_eq!(source.reads.load(Ordering::Relaxed), 1);
    }
}
