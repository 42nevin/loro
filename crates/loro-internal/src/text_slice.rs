//! Text payloads may be native bytes or logical extents. Extents never cache bytes.
use append_only_bytes::BytesSlice;
use std::{
    borrow::Cow,
    fmt, io,
    ops::{Bound, Range, RangeBounds},
    sync::Arc,
};

/// Experimental per-thread counters for actual temporary payload copies.
/// Reset/snapshot on the edit thread; output serialization and native-owner
/// allocation are separate harness stages. No counters are enabled by default.
#[cfg(feature = "shared_arena_experiment")]
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct TextCopyCount {
    pub native_bytes: u64,
    pub extent_bytes: u64,
    pub native_calls: u64,
    pub extent_calls: u64,
}
#[cfg(feature = "shared_arena_experiment")]
thread_local! {
    static TEXT_COPIES: std::cell::RefCell<std::collections::BTreeMap<&'static str, TextCopyCount>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}
#[cfg(feature = "shared_arena_experiment")]
pub fn reset_text_copy_metrics() {
    TEXT_COPIES.with(|counts| counts.borrow_mut().clear());
}
#[cfg(feature = "shared_arena_experiment")]
pub fn text_copy_metrics() -> std::collections::BTreeMap<&'static str, TextCopyCount> {
    TEXT_COPIES.with(|counts| counts.borrow().clone())
}
pub(crate) fn record_copy(consumer: &'static str, bytes: usize, extent: bool) {
    #[cfg(feature = "shared_arena_experiment")]
    TEXT_COPIES.with(|counts| {
        let mut counts = counts.borrow_mut();
        let count = counts.entry(consumer).or_default();
        if extent {
            count.extent_bytes += bytes as u64;
            count.extent_calls += 1;
        } else {
            count.native_bytes += bytes as u64;
            count.native_calls += 1;
        }
    });
    #[cfg(not(feature = "shared_arena_experiment"))]
    let _ = (consumer, bytes, extent);
}

/// Resolve immutable extents inside a bounded storage session. A view may not escape.
/// Implementors retain the original extent while any resolver-backed handle,
/// recoverable draft or published history references it. Reusing an ID for other
/// bytes is invalid. Sessions may be explicitly reused across one edit/export;
/// the resolver must not keep a transaction alive merely because a handle exists.
/// `visit` must execute exactly once on the requested range, before returning.
/// Native generated insertion can nest immutable resolver visits (for example,
/// transaction application calls into rich-text chunk validation). A resolver
/// must permit these nested read callbacks without an exclusive non-reentrant
/// lock. The experiment's LMDB adapter uses reentrant locking with shared state
/// borrows; callers must not mutate or close the underlying session during a view.
/// Fallible allocation/recovery boundaries use `try_with_bytes`; legacy native
/// infallible consumers treat a lost/missing immutable extent as an invariant
/// failure. Such candidates must not be published after resolver failure.
pub trait ExtentResolver: Send + Sync {
    fn with_bytes(
        &self,
        extent: u64,
        range: Range<usize>,
        consumer: &'static str,
        visit: &mut dyn FnMut(&[u8]),
    ) -> io::Result<()>;
    fn record_materialization(
        &self,
        _extent: u64,
        _range: Range<usize>,
        _consumer: &'static str,
        _bytes: usize,
    ) {
    }
}

/// Metadata-only storage proof. Diagnostic scans are outside measured edit paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBackingInfo {
    pub kind: &'static str,
    pub extent: Option<u64>,
    pub range: Range<usize>,
}

/// Correctness-only scan; never include its history walk in edit timings.
#[derive(Clone, Debug, Default)]
pub struct TextBackingSnapshot {
    pub arena: Vec<TextBackingInfo>,
    pub ops: Vec<TextBackingInfo>,
    pub state: Vec<TextBackingInfo>,
    pub unloaded_containers: usize,
    pub unloaded_containers_unknown: bool,
}

/// Immutable metadata for a concatenation of extents from one resolver.
/// Fields are private: all parts are nonempty and the checked total is cached.
#[cfg(feature = "shared_arena_experiment")]
pub struct ExtentSequence {
    resolver: Arc<dyn ExtentResolver>,
    parts: Vec<(u64, Range<usize>)>,
    byte_len: usize,
}
#[cfg(feature = "shared_arena_experiment")]
impl fmt::Debug for ExtentSequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtentSequence").field("parts", &self.parts).field("byte_len", &self.byte_len).finish()
    }
}
#[derive(Clone)]
pub enum TextSlice {
    Native(BytesSlice),
    #[cfg(feature = "shared_arena_experiment")]
    ExtentSequence(Arc<ExtentSequence>),
    Extent {
        resolver: Arc<dyn ExtentResolver>,
        extent: u64,
        range: Range<usize>,
    },
}
impl fmt::Debug for TextSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(x) => x.fmt(f),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(x) => x.fmt(f),
            Self::Extent { extent, range, .. } => f
                .debug_struct("Extent")
                .field("id", extent)
                .field("range", range)
                .finish(),
        }
    }
}
impl From<BytesSlice> for TextSlice {
    fn from(x: BytesSlice) -> Self {
        Self::Native(x)
    }
}
impl PartialEq for TextSlice {
    fn eq(&self, other: &Self) -> bool {
        if self.ptr_eq(other) && self.start() == other.start() && self.end() == other.end() {
            return true;
        }
        if self.extent_resolver().zip(other.extent_resolver()).is_some_and(|(a,b)| !Arc::ptr_eq(a,b)) {
            // Release the first store's view before entering another store.
            let left = self.materialize("text.equal.cross_store");
            return other.with_bytes("text.equal", |right| left.as_ref() == right);
        }
        self.with_bytes("text.equal", |a| other.with_bytes("text.equal", |b| a == b))
    }
}
impl Eq for TextSlice {}
impl TextSlice {
    pub fn backing_info(&self) -> TextBackingInfo {
        match self {
            Self::Native(x) => TextBackingInfo {kind:"native",extent:None,range:x.start()..x.end()},
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(x) => TextBackingInfo {kind:"extent_sequence",extent:None,range:0..x.byte_len},
            Self::Extent{extent,range,..} => TextBackingInfo {kind:"extent",extent:Some(*extent),range:range.clone()},
        }
    }

    /// Ordered physical ranges, never a fabricated contiguous extent identity.
    pub fn backing_parts(&self) -> Vec<TextBackingInfo> {
        #[cfg(feature = "shared_arena_experiment")]
        if let Self::ExtentSequence(sequence) = self {
            return sequence.parts.iter().map(|(id, range)| TextBackingInfo {
                kind: "extent", extent: Some(*id), range: range.clone(),
            }).collect();
        }
        vec![self.backing_info()]
    }
    fn extent_resolver(&self) -> Option<&Arc<dyn ExtentResolver>> {
        match self {
            Self::Extent { resolver, .. } => Some(resolver),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(sequence) => Some(&sequence.resolver),
            _ => None,
        }
    }
    /// Representation identity, not byte equality. Used by diagnostic lineage.
    #[cfg(feature = "shared_arena_experiment")]
    pub(crate) fn same_backing(&self, other: &Self) -> bool {
        if let Some((a,b)) = self.extent_resolver().zip(other.extent_resolver()) {
            return Arc::ptr_eq(a,b) && self.backing_parts() == other.backing_parts();
        }
        self.ptr_eq(other) && self.start() == other.start() && self.end() == other.end()
    }
    #[cfg(feature = "shared_arena_experiment")]
    fn from_extent_parts(resolver: Arc<dyn ExtentResolver>, mut parts: Vec<(u64, Range<usize>)>) -> Self {
        parts.retain(|(_, range)| !range.is_empty());
        if parts.len() == 1 {
            let (extent, range) = parts.pop().unwrap();
            return Self::extent(resolver, extent, range);
        }
        let byte_len = parts.iter().try_fold(0usize, |n, (_, r)| n.checked_add(r.len())).expect("extent sequence length overflow");
        Self::ExtentSequence(Arc::new(ExtentSequence { resolver, parts, byte_len }))
    }
    #[cfg(feature = "shared_arena_experiment")]
    fn join_extents(sequence: &ExtentSequence, consumer: &'static str) -> io::Result<Vec<u8>> {
        let mut output = Vec::with_capacity(sequence.byte_len);
        for (extent, range) in &sequence.parts {
            let part = Self::extent(sequence.resolver.clone(), *extent, range.clone());
            let mut copied = 0;
            let result = part.try_with_bytes(consumer, |bytes| {
                output.extend_from_slice(bytes);
                copied = bytes.len();
                // Count the actual temporary copy even if resolution later fails
                // or panics after invoking its visitor.
                record_copy("text_slice.extent_sequence_join", bytes.len(), true);
            });
            // Match the existing single-extent materialization contract: metric
            // callbacks run after the resolver has released its bounded view.
            if copied != 0 {
                sequence.resolver.record_materialization(*extent, range.clone(),
                    "text_slice.extent_sequence_join", copied);
            }
            result?;
        }
        Ok(output)
    }

    pub fn extent(resolver: Arc<dyn ExtentResolver>, extent: u64, range: Range<usize>) -> Self {
        assert!(range.start <= range.end);
        Self::Extent {
            resolver,
            extent,
            range,
        }
    }
    pub fn empty() -> Self {
        Self::Native(BytesSlice::empty())
    }
    pub fn from_bytes(x: &[u8]) -> Self {
        Self::Native(BytesSlice::from_bytes(x))
    }
    pub fn len(&self) -> usize {
        match self {
            Self::Native(x) => x.len(),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(x) => x.byte_len,
            Self::Extent { range, .. } => range.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn start(&self) -> usize {
        match self {
            Self::Native(x) => x.start(),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(_) => 0,
            Self::Extent { range, .. } => range.start,
        }
    }
    pub fn end(&self) -> usize {
        self.start() + self.len()
    }
    pub fn is_extent(&self) -> bool {
        self.extent_resolver().is_some()
    }
    pub fn try_with_bytes<R>(
        &self,
        consumer: &'static str,
        f: impl FnOnce(&[u8]) -> R,
    ) -> io::Result<R> {
        match self {
            Self::Native(x) => Ok(f(x)),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(sequence) => {
                let joined = Self::join_extents(sequence, consumer)?;
                Ok(f(&joined))
            }
            Self::Extent {
                resolver,
                extent,
                range,
            } => {
                let mut f = Some(f);
                let mut value = None;
                let mut bad = false;
                resolver.with_bytes(*extent, range.clone(), consumer, &mut |bytes| {
                    if bytes.len() != range.len() || f.is_none() {
                        bad = true;
                        return;
                    }
                    value = Some(f.take().unwrap()(bytes));
                })?;
                if bad {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid extent resolver callback",
                    ));
                }
                value.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "extent resolver did not provide bytes",
                    )
                })
            }
        }
    }
    pub fn with_bytes<R>(&self, consumer: &'static str, f: impl FnOnce(&[u8]) -> R) -> R {
        self.try_with_bytes(consumer, f)
            .expect("text extent resolution failed")
    }
    pub fn with_str<R>(&self, consumer: &'static str, f: impl FnOnce(&str) -> R) -> R {
        self.with_bytes(consumer, |bytes| {
            f(std::str::from_utf8(bytes).expect("validated text extent"))
        })
    }
    pub fn materialize(&self, consumer: &'static str) -> Cow<'_, [u8]> {
        match self {
            Self::Native(x) => Cow::Borrowed(x),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(sequence) => Cow::Owned(Self::join_extents(sequence, consumer)
                .expect("text extent resolution failed")),
            Self::Extent {
                resolver,
                extent,
                range,
            } => {
                let bytes = self.with_bytes(consumer, |x| x.to_vec());
                record_copy(consumer, bytes.len(), true);
                resolver.record_materialization(*extent, range.clone(), consumer, bytes.len());
                Cow::Owned(bytes)
            }
        }
    }
    pub fn record_materialization(&self, consumer: &'static str, bytes: usize) {
        record_copy(consumer, bytes, self.is_extent());
        #[cfg(feature = "shared_arena_experiment")]
        if let Self::ExtentSequence(sequence) = self {
            let mut remaining = bytes;
            for (extent, range) in &sequence.parts {
                let count = remaining.min(range.len());
                if count != 0 { sequence.resolver.record_materialization(*extent, range.clone(), consumer, count); }
                remaining -= count;
            }
        }
        if let Self::Extent {
            resolver,
            extent,
            range,
        } = self
        {
            resolver.record_materialization(*extent, range.clone(), consumer, bytes);
        }
    }
    pub fn as_bytes(&self) -> Cow<'_, [u8]> {
        self.materialize("text_slice.as_bytes")
    }
    pub fn as_str(&self) -> Cow<'_, str> {
        self.as_str_for("text_slice.as_str")
    }
    pub fn str_range(&self, range: Range<usize>, consumer: &'static str) -> Cow<'_, str> {
        match self {
            Self::Native(x) => {
                Cow::Borrowed(std::str::from_utf8(&x[range]).expect("valid text range"))
            }
            _ => Cow::Owned(self.slice_clone(range).as_str_for(consumer).into_owned()),
        }
    }
    pub fn as_str_for(&self, consumer: &'static str) -> Cow<'_, str> {
        match self.materialize(consumer) {
            Cow::Borrowed(x) => Cow::Borrowed(std::str::from_utf8(x).expect("validated text")),
            Cow::Owned(x) => Cow::Owned(String::from_utf8(x).expect("validated text")),
        }
    }
    pub fn to_vec(&self) -> Vec<u8> {
        self.to_vec_for("text_slice.to_vec")
    }
    /// Materialize one owned output buffer and count the copy for either backing.
    pub fn to_vec_for(&self, consumer: &'static str) -> Vec<u8> {
        match self {
            Self::Native(bytes) => {
                let output = bytes.to_vec();
                record_copy(consumer, output.len(), false);
                output
            }
            _ => self.materialize(consumer).into_owned(),
        }
    }
    pub fn slice_clone(&self, range: impl RangeBounds<usize>) -> Self {
        let start = match range.start_bound() {
            Bound::Included(x) => *x,
            Bound::Excluded(x) => x + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(x) => x + 1,
            Bound::Excluded(x) => *x,
            Bound::Unbounded => self.len(),
        };
        assert!(start <= end && end <= self.len());
        match self {
            Self::Native(x) => Self::Native(x.slice_clone(start..end)),
            #[cfg(feature = "shared_arena_experiment")]
            Self::ExtentSequence(sequence) => {
                if start == 0 && end == sequence.byte_len { return self.clone(); }
                let mut offset = 0;
                let mut parts = Vec::new();
                for (extent, range) in &sequence.parts {
                    let part_end = offset + range.len();
                    let lo = start.max(offset);
                    let hi = end.min(part_end);
                    if lo < hi { parts.push((*extent, range.start + (lo - offset)..range.start + (hi - offset))); }
                    offset = part_end;
                    if offset >= end { break; }
                }
                Self::from_extent_parts(sequence.resolver.clone(), parts)
            }
            Self::Extent {
                resolver,
                extent,
                range,
            } => Self::Extent {
                resolver: resolver.clone(),
                extent: *extent,
                range: range.start + start..range.start + end,
            },
        }
    }
    pub fn slice_(&mut self, r: impl RangeBounds<usize>) {
        *self = self.slice_clone(r);
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(a), Self::Native(b)) => a.ptr_eq(b),
            #[cfg(feature = "shared_arena_experiment")]
            (Self::ExtentSequence(a), Self::ExtentSequence(b)) => Arc::ptr_eq(a,b),
            (
                Self::Extent {
                    resolver: a,
                    extent: ai,
                    ..
                },
                Self::Extent {
                    resolver: b,
                    extent: bi,
                    ..
                },
            ) => ai == bi && Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    pub fn can_merge(&self, other: &Self) -> bool {
        #[cfg(feature = "shared_arena_experiment")]
        if let Some((a,b)) = self.extent_resolver().zip(other.extent_resolver()) {
            return Arc::ptr_eq(a,b);
        }
        self.ptr_eq(other) && self.end() == other.start()
    }
    pub fn try_merge(&mut self, other: &Self) -> Result<(), append_only_bytes::MergeFailed> {
        if !self.can_merge(other) {
            return Err(append_only_bytes::MergeFailed);
        }
        #[cfg(feature = "shared_arena_experiment")]
        if let Some(resolver) = self.extent_resolver().cloned() {
            // Preserve the existing contiguous single-extent fast path.
            if let (Self::Extent { extent: a, range: ar, .. }, Self::Extent { extent: b, range: br, .. }) = (&mut *self, other) {
                if a == b && ar.end == br.start { ar.end = br.end; return Ok(()); }
            }
            let parts = self.backing_parts().into_iter().chain(other.backing_parts())
                .map(|p| (p.extent.expect("extent-backed sequence"), p.range)).collect();
            *self = Self::from_extent_parts(resolver, parts);
            return Ok(());
        }
        match (self, other) {
            (Self::Native(a), Self::Native(b)) => a.try_merge(b),
            (Self::Extent { range: a, .. }, Self::Extent { range: b, .. }) => {
                a.end = b.end;
                Ok(())
            }
            _ => unreachable!(),
        }
    }
    /// Native-only diagnostic; extent addresses cannot escape their session.
    pub fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Native(x) => x.as_ptr(),
            _ => panic!("logical extent has no persistent pointer"),
        }
    }
}
impl rle::HasLength for TextSlice {
    fn content_len(&self) -> usize {
        self.len()
    }
}
impl rle::Sliceable for TextSlice {
    fn slice(&self, from: usize, to: usize) -> Self {
        self.slice_clone(from..to)
    }
}
impl rle::Mergable for TextSlice {
    fn is_mergable(&self, other: &Self, _: &()) -> bool {
        self.can_merge(other)
    }
    fn merge(&mut self, other: &Self, _: &()) {
        self.try_merge(other).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Resolver {
        bytes: Vec<u8>,
        reads: AtomicUsize,
        copied: AtomicUsize,
    }
    impl ExtentResolver for Resolver {
        fn with_bytes(
            &self,
            _: u64,
            range: Range<usize>,
            _: &'static str,
            visit: &mut dyn FnMut(&[u8]),
        ) -> io::Result<()> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            visit(&self.bytes[range]);
            Ok(())
        }
        fn record_materialization(&self, _: u64, _: Range<usize>, _: &'static str, n: usize) {
            self.copied.fetch_add(n, Ordering::SeqCst);
        }
    }
    fn resolver() -> Arc<Resolver> {
        Arc::new(Resolver {
            bytes: b"abcd".to_vec(),
            reads: AtomicUsize::new(0),
            copied: AtomicUsize::new(0),
        })
    }
    #[test]
    fn logical_handle_slicing_is_metadata_only_and_cross_store_equality_is_counted() {
        let a = resolver();
        let b = resolver();
        let x = TextSlice::extent(a.clone(), 7, 0..4);
        let first = x.slice_clone(..2);
        let second = x.slice_clone(2..);
        assert_eq!(a.reads.load(Ordering::SeqCst), 0);
        let mut merged = first;
        merged.try_merge(&second).unwrap();
        assert_eq!(merged, x);
        assert_eq!(a.reads.load(Ordering::SeqCst), 0);
        assert_eq!(x, TextSlice::extent(b.clone(), 7, 0..4));
        assert_eq!(a.copied.load(Ordering::SeqCst), 4);
        assert_eq!(b.copied.load(Ordering::SeqCst), 0);
        assert!(x.is_extent());
    }
    #[cfg(feature = "shared_arena_experiment")]
    #[test]
    fn universal_counters_distinguish_borrow_from_real_copy_for_both_backings() {
        reset_text_copy_metrics();
        let native = TextSlice::from_bytes(b"abcd");
        let logical = TextSlice::extent(resolver(), 9, 0..4);
        assert!(matches!(native.materialize("borrow"), Cow::Borrowed(_)));
        assert!(text_copy_metrics().is_empty());
        assert_eq!(native.to_vec(), logical.to_vec());
        let counts = text_copy_metrics();
        let copies = &counts["text_slice.to_vec"];
        assert_eq!((copies.native_bytes, copies.extent_bytes), (4, 4));
        assert_eq!((copies.native_calls, copies.extent_calls), (1, 1));
        for slice in [&native, &logical] {
            let joined = slice.with_bytes("arena.cross_extent_join", |bytes| bytes.to_vec());
            slice.record_materialization("arena.cross_extent_join", joined.len());
        }
        let counts = text_copy_metrics();
        let copies = &counts["arena.cross_extent_join"];
        assert_eq!((copies.native_bytes, copies.extent_bytes), (4, 4));
        reset_text_copy_metrics();
        assert!(text_copy_metrics().is_empty());
        assert_eq!(native.to_vec_for("oplog.raw_op"), logical.to_vec_for("oplog.raw_op"));
        let counts = text_copy_metrics();
        assert_eq!(counts["oplog.raw_op"].native_bytes, 4);
        assert_eq!(counts["oplog.raw_op"].extent_bytes, 4);
    }
    #[test]
    fn fallible_view_rejects_missing_or_wrong_length_without_calling_consumer() {
        struct Bad;
        impl ExtentResolver for Bad {
            fn with_bytes(
                &self,
                id: u64,
                _: Range<usize>,
                _: &'static str,
                v: &mut dyn FnMut(&[u8]),
            ) -> io::Result<()> {
                if id == 0 {
                    return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
                }
                v(b"wrong");
                Ok(())
            }
        }
        for id in [0, 1] {
            let x = TextSlice::extent(Arc::new(Bad), id, 0..2);
            assert!(x
                .try_with_bytes("test", |_| panic!("consumer must not run"))
                .is_err());
        }
    }
}
