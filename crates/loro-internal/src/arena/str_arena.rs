use std::ops::{Bound, RangeBounds};

use crate::text_slice::TextSlice as BytesSlice;
use append_only_bytes::AppendOnlyBytes;

use crate::container::richtext::richtext_state::unicode_to_utf8_index;
const INDEX_INTERVAL: u32 = 128;

#[derive(Default, Debug, Clone)]
pub(crate) struct StrArena {
    bytes: AppendOnlyBytes,
    // Sealed native buffers with global byte offsets. No mutable append handle
    // survives sealing; each owner has its own tail and segment table.
    sealed: im::Vector<Segment>,
    sealed_len: usize,
    unicode_indexes: im::Vector<Index>,
    len: Index,
}

#[derive(Debug, Clone)]
pub(crate) struct StrArenaCheckpoint {
    bytes_len: usize,
    unicode_indexes_len: usize,
    len: Index,
}

#[derive(Debug, Default)]
struct Index {
    bytes: u32,
    utf16: u32,
    unicode: u32,
}

impl Clone for Index {
    fn clone(&self) -> Self {
        #[cfg(any(test, feature = "test_utils"))]
        crate::cow_work::index_clone();
        Self {
            bytes: self.bytes,
            utf16: self.utf16,
            unicode: self.unicode,
        }
    }
}
impl Index {
    fn scalar_copy(&self) -> Self {
        #[cfg(any(test, feature = "test_utils"))]
        crate::cow_work::scalar_index_clone();
        Self {
            bytes: self.bytes,
            utf16: self.utf16,
            unicode: self.unicode,
        }
    }
}

/// Immutable payload and its absolute arena offset. Persistent tree writes
/// detach only their path; no owner shares a mutable append tail.
#[derive(Debug)]
struct Segment(usize, BytesSlice);
impl Clone for Segment {
    fn clone(&self) -> Self {
        #[cfg(any(test, feature = "test_utils"))]
        crate::cow_work::segment_clone();
        Self(self.0, self.1.clone())
    }
}

impl StrArenaCheckpoint {
    pub(super) fn bytes_len(&self) -> usize {
        self.bytes_len
    }
}

impl StrArena {
    pub(crate) fn fork_cow(&mut self) -> Self {
        if !self.bytes.is_empty() {
            let bytes = std::mem::take(&mut self.bytes);
            let length = bytes.len();
            self.sealed
                .push_back(Segment(self.sealed_len, bytes.slice(..).into()));
            self.sealed_len += length;
        }
        Self {
            bytes: AppendOnlyBytes::default(),
            sealed: self.sealed.clone(),
            sealed_len: self.sealed_len,
            unicode_indexes: self.unicode_indexes.clone(),
            len: self.len.scalar_copy(),
        }
    }

    pub(super) fn bytes_len(&self) -> usize {
        self.len.bytes as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len.bytes == 0
    }

    #[inline]
    #[allow(dead_code)]
    pub fn len_bytes(&self) -> usize {
        self.len.bytes as usize
    }

    #[inline]
    pub fn len_utf16(&self) -> usize {
        self.len.utf16 as usize
    }

    #[inline]
    pub fn len_unicode(&self) -> usize {
        self.len.unicode as usize
    }

    pub fn alloc(&mut self, input: &str) {
        let mut utf16 = 0;
        let mut unicode_len = 0;
        let mut last_save_index = 0;
        for (byte_index, c) in input.char_indices() {
            let byte_index = byte_index + c.len_utf8();
            utf16 += c.len_utf16() as u32;
            unicode_len += 1;
            if byte_index - last_save_index > INDEX_INTERVAL as usize {
                self._alloc(&input[last_save_index..byte_index], utf16, unicode_len);
                last_save_index = byte_index;
                utf16 = 0;
                unicode_len = 0;
            }
        }

        if last_save_index != input.len() {
            self._alloc(&input[last_save_index..], utf16, unicode_len);
        }
    }

    /// Adopt an immutable native owner without another payload copy. The storage
    /// allocator may already have copied out of a bounded database write guard.
    pub(crate) fn alloc_sealed(&mut self, slice: BytesSlice) {
        if slice.is_empty() {
            return;
        }
        if !self.bytes.is_empty() {
            let bytes = std::mem::take(&mut self.bytes);
            let length = bytes.len();
            self.sealed
                .push_back(Segment(self.sealed_len, bytes.slice(..).into()));
            self.sealed_len += length;
        }
        slice.with_str("arena.adopt.index", |input| {
            if self.unicode_indexes.is_empty() {
                self.unicode_indexes.push_back(Index::default());
            }
            for c in input.chars() {
                self.len.bytes += c.len_utf8() as u32;
                self.len.utf16 += c.len_utf16() as u32;
                self.len.unicode += 1;
                if self.len.bytes - self.unicode_indexes.back().unwrap().bytes > INDEX_INTERVAL {
                    self.unicode_indexes.push_back(self.len.scalar_copy());
                }
            }
        });
        self.sealed_len += slice.len();
        self.sealed
            .push_back(Segment(self.sealed_len - slice.len(), slice));
    }

    fn _alloc(&mut self, input: &str, utf16: u32, unicode_len: i32) {
        let s = input;
        self.len.bytes += s.len() as u32;
        self.len.utf16 += utf16;
        self.len.unicode += unicode_len as u32;
        self.bytes.push_str(s);
        let cur_len = self.len.scalar_copy();

        let index = &mut self.unicode_indexes;
        if index.is_empty() {
            index.push_back(Index {
                bytes: 0,
                utf16: 0,
                unicode: 0,
            });
        }

        let last = index.back().unwrap();
        if cur_len.bytes - last.bytes > INDEX_INTERVAL {
            self.unicode_indexes.push_back(cur_len);
        }
    }

    #[inline]
    pub fn slice_by_unicode(&mut self, range: impl RangeBounds<usize>) -> BytesSlice {
        let (start, end) = self.unicode_range_to_utf8_range(range);
        self.slice_bytes(start..end)
    }

    #[inline]
    pub fn slice_str_by_unicode(
        &mut self,
        range: impl RangeBounds<usize>,
    ) -> std::borrow::Cow<'_, str> {
        let (start, end) = self.unicode_range_to_utf8_range(range);
        if start >= self.sealed_len {
            return std::borrow::Cow::Borrowed(
                std::str::from_utf8(&self.bytes[start - self.sealed_len..end - self.sealed_len])
                    .unwrap(),
            );
        }
        let first = self.segment_boundary(start, true).saturating_sub(1);
        let Segment(offset, bytes) = &self.sealed[first];
        if end <= offset + bytes.len() {
            return bytes.str_range(start - offset..end - offset, "arena.slice_str");
        }
        std::borrow::Cow::Owned(String::from_utf8(self.slice_bytes(start..end).to_vec()).unwrap())
    }

    fn unicode_range_to_utf8_range(&mut self, range: impl RangeBounds<usize>) -> (usize, usize) {
        if self.is_empty() {
            return (0, 0);
        }

        let start = match range.start_bound() {
            Bound::Included(&i) => self.unicode_to_byte_index(i as u32),
            Bound::Excluded(&_i) => unreachable!(),
            Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            Bound::Included(&i) => self.unicode_to_byte_index(i as u32 + 1),
            Bound::Excluded(&i) => self.unicode_to_byte_index(i as u32),
            Bound::Unbounded => self.len.bytes as usize,
        };

        (start, end)
    }

    #[inline]
    pub fn slice_bytes(&self, range: impl RangeBounds<usize>) -> BytesSlice {
        let start = match range.start_bound() {
            Bound::Included(&i) => i,
            Bound::Excluded(&i) => i + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&i) => i + 1,
            Bound::Excluded(&i) => i,
            Bound::Unbounded => self.len.bytes as usize,
        };
        assert!(start <= end && end <= self.len.bytes as usize);
        if start >= self.sealed_len {
            return self
                .bytes
                .slice(start - self.sealed_len..end - self.sealed_len)
                .into();
        }
        let first = self.segment_boundary(start, true).saturating_sub(1);
        let Segment(offset, bytes) = &self.sealed[first];
        if end <= offset + bytes.len() {
            return bytes.slice_clone(start - offset..end - offset);
        }
        // Only a requested cross-segment slice is copied, never the whole prefix.
        let mut joined = AppendOnlyBytes::with_capacity(end - start);
        // Index directly into the requested range, without walking the prefix.
        for position in first..self.sealed.len() {
            let Segment(offset, bytes) = &self.sealed[position];
            #[cfg(any(test, feature = "test_utils"))]
            crate::cow_work::segment_visit();
            if *offset >= end {
                break;
            }
            let lo = start.saturating_sub(*offset);
            let hi = (end - offset).min(bytes.len());
            let part = bytes.slice_clone(lo..hi);
            part.with_bytes("arena.cross_extent_join", |part| joined.push_slice(part));
            part.record_materialization("arena.cross_extent_join", hi - lo);
        }
        if end > self.sealed_len {
            joined.push_slice(&self.bytes[..end - self.sealed_len]);
            crate::text_slice::record_copy("arena.cross_extent_join", end - self.sealed_len, false);
        }
        joined.slice(..).into()
    }

    /// First segment whose offset is greater than `offset` (inclusive=true)
    /// or at least `offset` (inclusive=false). Offsets are strictly increasing.
    fn segment_boundary(&self, offset: usize, inclusive: bool) -> usize {
        let mut low = 0;
        let mut high = self.sealed.len();
        while low < high {
            let mid = low + (high - low) / 2;
            #[cfg(any(test, feature = "test_utils"))]
            crate::cow_work::segment_visit();
            let start = self.sealed[mid].0;
            if start < offset || (inclusive && start == offset) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    #[cfg(feature = "shared_arena_experiment")]
    pub(crate) fn inspect_text_backings(&self) -> Vec<crate::text_slice::TextBackingInfo> {
        let mut values:Vec<_> = self.sealed.iter().map(|segment|segment.1.backing_info()).collect();
        if !self.bytes.is_empty() {
            values.push(crate::text_slice::TextBackingInfo{kind:"native",extent:None,range:0..self.bytes.len()});
        }
        values
    }

    fn unicode_to_byte_index(&self, unicode_index: u32) -> usize {
        let i = match self
            .unicode_indexes
            .binary_search_by_key(&unicode_index, |x| x.unicode)
        {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let index = &self.unicode_indexes[i];
        if index.unicode == unicode_index {
            return index.bytes as usize;
        }
        let end = self
            .unicode_indexes
            .get(i + 1)
            .map_or(self.len.bytes, |x| x.bytes) as usize;
        let bytes = self.slice_bytes(index.bytes as usize..end);
        bytes.with_str("arena.unicode_index", |text| {
            unicode_to_utf8_index(text, (unicode_index - index.unicode) as usize).unwrap()
                + index.bytes as usize
        })
    }

    #[inline]
    pub(crate) fn checkpoint(&self) -> StrArenaCheckpoint {
        StrArenaCheckpoint {
            bytes_len: self.len.bytes as usize,
            unicode_indexes_len: self.unicode_indexes.len(),
            len: self.len.scalar_copy(),
        }
    }

    pub(crate) fn rollback(&mut self, checkpoint: StrArenaCheckpoint) {
        if checkpoint.bytes_len == 0 {
            *self = Self::default();
            return;
        }

        if checkpoint.bytes_len < self.len.bytes as usize {
            if checkpoint.bytes_len < self.sealed_len {
                // A rollback may cross a sealing boundary held by another branch.
                // Trim only our segment references; its immutable prefix survives.
                let retained = self.segment_boundary(checkpoint.bytes_len, false);
                self.sealed.truncate(retained);
                if let Some(Segment(offset, bytes)) = self.sealed.back_mut() {
                    *bytes = bytes.slice_clone(..checkpoint.bytes_len - *offset);
                }
                self.sealed_len = checkpoint.bytes_len;
                self.bytes = AppendOnlyBytes::default();
            } else {
                let count = checkpoint.bytes_len - self.sealed_len;
                let mut bytes = AppendOnlyBytes::with_capacity(count);
                bytes.push_slice(&self.bytes[..count]);
                self.bytes = bytes;
            }
        }
        self.unicode_indexes.truncate(
            checkpoint
                .unicode_indexes_len
                .min(self.unicode_indexes.len()),
        );
        self.len = checkpoint.len;
    }
}

#[cfg(test)]
mod test {

    use super::*;

    #[cfg(feature = "shared_arena_experiment")]
    #[test]
    fn generated_sealed_owner_rollback_keeps_other_branch() {
        let mut arena = StrArena::default();
        let first = BytesSlice::from_bytes("a界".as_bytes());
        arena.alloc_sealed(first.clone());
        assert!(arena.slice_bytes(..).ptr_eq(&first));
        let checkpoint = arena.checkpoint();
        arena.alloc_sealed(BytesSlice::from_bytes("🙂玉".as_bytes()));
        assert!(
            arena.slice_bytes(..first.len()).ptr_eq(&first),
            "old prefix was copied"
        );
        let mut branch = arena.fork_cow();
        arena.rollback(checkpoint);
        arena.alloc_sealed(BytesSlice::from_bytes("Z".as_bytes()));
        assert_eq!(arena.slice_str_by_unicode(..), "a界Z");
        assert_eq!(branch.slice_str_by_unicode(..), "a界🙂玉");
        assert_eq!(arena.len_utf16(), 3);
        assert_eq!(branch.len_utf16(), 5);
    }

    #[cfg(feature = "shared_arena_experiment")]
    #[test]
    fn external_sealed_owner_rollback_retains_pointer_and_branch() {
        use std::sync::Arc;
        struct Owner(Box<[u8]>);
        // SAFETY: exclusive boxed allocation, never mutated after construction.
        unsafe impl append_only_bytes::ImmutableBytes for Owner {
            fn as_bytes(&self) -> &[u8] {
                &self.0
            }
        }
        let external = |s: &str| -> BytesSlice {
            append_only_bytes::BytesSlice::from_immutable(Arc::new(Owner(s.as_bytes().into())))
                .into()
        };
        let first = external("a界");
        let second = external("🙂玉");
        let first_ptr = first.as_ptr();
        let second_ptr = second.as_ptr();
        let mut arena = StrArena::default();
        arena.alloc_sealed(first);
        let checkpoint = arena.checkpoint();
        arena.alloc_sealed(second);
        let mut branch = arena.fork_cow();
        arena.rollback(checkpoint);
        arena.alloc_sealed(external("Z"));
        assert_eq!(arena.slice_bytes(..4).as_ptr(), first_ptr);
        assert_eq!(branch.slice_bytes(4..11).as_ptr(), second_ptr);
        assert_eq!(arena.slice_str_by_unicode(..), "a界Z");
        assert_eq!(branch.slice_str_by_unicode(..), "a界🙂玉");
        drop(arena);
        assert_eq!(branch.slice_bytes(..4).as_ptr(), first_ptr);
        assert_eq!(branch.slice_str_by_unicode(..), "a界🙂玉");
    }

    #[cfg(feature = "shared_arena_experiment")]
    #[test]
    fn cross_extent_copy_metrics_include_native_sealed_and_mutable_tail() {
        use crate::text_slice::{ExtentResolver, TextSlice, reset_text_copy_metrics, text_copy_metrics};
        use std::{sync::Arc, ops::Range};
        struct Resolver;
        impl ExtentResolver for Resolver {
            fn with_bytes(&self, _:u64, range:Range<usize>, _: &'static str, visit:&mut dyn FnMut(&[u8])) -> std::io::Result<()> {
                visit(&b"a"[range]); Ok(())
            }
        }
        for logical in [false, true] {
            let mut arena=StrArena::default();
            arena.alloc_sealed(if logical {TextSlice::extent(Arc::new(Resolver),1,0..1)} else {TextSlice::from_bytes(b"a")});
            arena.alloc("b");
            reset_text_copy_metrics();
            assert_eq!(arena.slice_bytes(..).with_bytes("test", |x| x==b"ab"), true);
            let counts=text_copy_metrics();
            let joined=&counts["arena.cross_extent_join"];
            assert_eq!(joined.native_bytes+joined.extent_bytes,2);
            assert_eq!(joined.extent_bytes,if logical {1} else {0});
            assert_eq!(joined.native_bytes,if logical {1} else {2});
        }
    }

    #[test]
    fn many_sealed_segments_seek_and_rollback_preserve_other_owners() {
        for total in [129, 1025] {
            let prefix = total - 2;
            let mut arena = StrArena::default();
            let segment = "界🙂a".repeat(64);
            let width = segment.chars().count();
            for _ in 0..prefix {
                arena.alloc(&segment);
                drop(arena.fork_cow());
            }
            assert!(arena.unicode_indexes.len() > 64);
            let checkpoint = arena.checkpoint();
            let mut older = arena.fork_cow();
            for _ in 0..2 {
                arena.alloc(&segment);
                drop(arena.fork_cow());
            }
            crate::cow_work::reset();
            let mut full = arena.fork_cow();
            let fork_work = crate::cow_work::take();
            crate::cow_work::reset();
            let last = arena
                .slice_str_by_unicode((total - 1) * width..total * width)
                .into_owned();
            let seek = crate::cow_work::take();
            assert_eq!(last, segment);
            crate::cow_work::reset();
            arena.rollback(checkpoint);
            let rollback = crate::cow_work::take();
            arena.alloc("!");
            assert_eq!(arena.slice_str_by_unicode(prefix * width..), "!");
            assert_eq!(older.len_unicode(), prefix * width);
            assert_eq!(older.slice_str_by_unicode((prefix - 1) * width..), segment);
            assert_eq!(full.len_unicode(), total * width);
            assert_eq!(full.slice_str_by_unicode((total - 1) * width..), segment);
            eprintln!("SEGMENT_WORK segments={total} seek_visits={} rollback_visits={} fork_index_clones={} fork_segment_clones={} rollback_index_clones={} rollback_segment_clones={}",
            seek.string_segment_visits, rollback.string_segment_visits,
            fork_work.string_index_entries_cloned, fork_work.string_segment_entries_cloned,
            rollback.string_index_entries_cloned, rollback.string_segment_entries_cloned);
            // Even 1025 segments need at most eleven search probes; allow extra
            // bounded indexing/boundary checks, but never a whole-prefix scan.
            assert!(
                seek.string_segment_visits <= 32,
                "seek scanned retained prefix"
            );
            assert!(
                rollback.string_segment_visits <= 32,
                "rollback scanned retained prefix"
            );
            assert!(fork_work.string_index_entries_cloned <= 128);
            assert!(fork_work.string_segment_entries_cloned <= 128);
            assert!(rollback.string_index_entries_cloned <= 512);
            assert!(rollback.string_segment_entries_cloned <= 512);
        }
    }

    #[test]
    fn sealed_prefixes_keep_unicode_slices_and_rollback_branch_local() {
        let mut a = StrArena::default();
        a.alloc("A界🙂Z");
        let mut b = a.fork_cow();
        let checkpoint = b.checkpoint();
        a.alloc("source");
        b.alloc("private");
        assert_eq!(
            a.slice_by_unicode(..).as_bytes().as_ref(),
            "A界🙂Zsource".as_bytes()
        );
        assert_eq!(
            b.slice_by_unicode(..).as_bytes().as_ref(),
            "A界🙂Zprivate".as_bytes()
        );
        let mut c = b.fork_cow();
        b.rollback(checkpoint);
        b.alloc("!");
        c.alloc("尾");
        assert_eq!(
            b.slice_by_unicode(1..).as_bytes().as_ref(),
            "界🙂Z!".as_bytes()
        );
        assert_eq!(
            c.slice_by_unicode(2..).as_bytes().as_ref(),
            "🙂Zprivate尾".as_bytes()
        );
        assert_eq!(
            a.slice_by_unicode(1..3).as_bytes().as_ref(),
            "界🙂".as_bytes()
        );
    }

    #[test]
    fn test() {
        let mut arena = StrArena::default();
        arena.alloc("Hello");
        let slice = arena.slice_by_unicode(0..5);
        assert_eq!(slice.as_bytes().as_ref(), b"Hello");
        arena.alloc("World");
        let slice = arena.slice_by_unicode(5..10);
        assert_eq!(slice.as_bytes().as_ref(), b"World");
    }

    #[test]
    fn parse_unicode_correctly() {
        let mut arena = StrArena::default();
        arena.alloc("Hello");
        arena.alloc("World");
        arena.alloc("你好");
        arena.alloc("世界");
        let slice = arena.slice_by_unicode(0..4);
        assert_eq!(slice.as_bytes().as_ref(), b"Hell");

        let slice = arena.slice_by_unicode(4..8);
        assert_eq!(slice.as_bytes().as_ref(), b"oWor");

        let slice = arena.slice_by_unicode(8..10);
        assert_eq!(slice.as_bytes().as_ref(), b"ld");

        let slice = arena.slice_by_unicode(10..12);
        assert_eq!(slice.as_bytes().as_ref(), "你好".as_bytes());

        let slice = arena.slice_by_unicode(12..14);
        assert_eq!(slice.as_bytes().as_ref(), "世界".as_bytes());
    }

    #[test]
    fn slice_long_unicode_correctly() {
        let mut arena = StrArena::default();
        let src = "一二34567八九零";
        for s in std::iter::repeat_n(src, 100) {
            arena.alloc(s);
        }

        let slice = arena.slice_by_unicode(110..120);
        assert_eq!(slice.as_bytes().as_ref(), src.as_bytes());
        let slice = arena.slice_by_unicode(111..121);
        assert_eq!(slice.as_bytes().as_ref(), "二34567八九零一".as_bytes());
    }

    #[test]
    fn rollback_truncates_bytes_without_losing_prefix() {
        let mut arena = StrArena::default();
        arena.alloc("hello");
        let checkpoint = arena.checkpoint();
        arena.alloc(" transient");
        arena.rollback(checkpoint);
        arena.alloc("!");

        let slice = arena.slice_by_unicode(..);
        assert_eq!(slice.as_bytes().as_ref(), b"hello!");
    }
}
