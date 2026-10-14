use std::ops::{Bound, RangeBounds};

use append_only_bytes::{AppendOnlyBytes, BytesSlice};

use crate::container::richtext::richtext_state::unicode_to_utf8_index;
const INDEX_INTERVAL: u32 = 128;

#[derive(Default, Debug, Clone)]
pub(crate) struct StrArena {
    bytes: AppendOnlyBytes,
    // Sealed native buffers with global byte offsets. No mutable append handle
    // survives sealing; each owner has its own tail and segment table.
    sealed: Vec<(usize, BytesSlice)>,
    sealed_len: usize,
    unicode_indexes: Vec<Index>,
    len: Index,
}

#[derive(Debug, Clone)]
pub(crate) struct StrArenaCheckpoint {
    bytes_len: usize,
    unicode_indexes_len: usize,
    len: Index,
}

#[derive(Debug, Default, Clone, Copy)]
struct Index {
    bytes: u32,
    utf16: u32,
    unicode: u32,
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
            self.sealed.push((self.sealed_len, bytes.slice(..)));
            self.sealed_len += length;
        }
        Self {
            bytes: AppendOnlyBytes::default(),
            sealed: self.sealed.clone(),
            sealed_len: self.sealed_len,
            unicode_indexes: self.unicode_indexes.clone(),
            len: self.len,
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

    fn _alloc(&mut self, input: &str, utf16: u32, unicode_len: i32) {
        let s = input;
        self.len.bytes += s.len() as u32;
        self.len.utf16 += utf16;
        self.len.unicode += unicode_len as u32;
        self.bytes.push_str(s);
        let cur_len = self.len;

        let index = &mut self.unicode_indexes;
        if index.is_empty() {
            index.push(Index {
                bytes: 0,
                utf16: 0,
                unicode: 0,
            });
        }

        let last = index.last().unwrap();
        if cur_len.bytes - last.bytes > INDEX_INTERVAL {
            self.unicode_indexes.push(cur_len);
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
        if let Some((offset, bytes)) = self
            .sealed
            .iter()
            .find(|(offset, bytes)| start >= *offset && end <= *offset + bytes.len())
        {
            return std::borrow::Cow::Borrowed(
                std::str::from_utf8(&bytes[start - offset..end - offset]).unwrap(),
            );
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
                .slice(start - self.sealed_len..end - self.sealed_len);
        }
        let first = self
            .sealed
            .partition_point(|(offset, _)| *offset <= start)
            .saturating_sub(1);
        let (offset, bytes) = &self.sealed[first];
        if end <= offset + bytes.len() {
            return bytes.slice_clone(start - offset..end - offset);
        }
        // Only a requested cross-segment slice is copied, never the whole prefix.
        let mut joined = AppendOnlyBytes::with_capacity(end - start);
        for (offset, bytes) in &self.sealed[first..] {
            if *offset >= end {
                break;
            }
            let lo = start.saturating_sub(*offset);
            let hi = (end - offset).min(bytes.len());
            joined.push_slice(&bytes[lo..hi]);
        }
        if end > self.sealed_len {
            joined.push_slice(&self.bytes[..end - self.sealed_len]);
        }
        joined.slice(..)
    }

    fn unicode_to_byte_index(&self, unicode_index: u32) -> usize {
        let i = match self
            .unicode_indexes
            .binary_search_by_key(&unicode_index, |x| x.unicode)
        {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let index = self.unicode_indexes[i];
        if index.unicode == unicode_index {
            return index.bytes as usize;
        }
        let end = self
            .unicode_indexes
            .get(i + 1)
            .map_or(self.len.bytes, |x| x.bytes) as usize;
        let bytes = self.slice_bytes(index.bytes as usize..end);
        let text = std::str::from_utf8(&bytes).unwrap();
        unicode_to_utf8_index(text, (unicode_index - index.unicode) as usize).unwrap()
            + index.bytes as usize
    }

    #[inline]
    pub(crate) fn checkpoint(&self) -> StrArenaCheckpoint {
        StrArenaCheckpoint {
            bytes_len: self.len.bytes as usize,
            unicode_indexes_len: self.unicode_indexes.len(),
            len: self.len,
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
                self.sealed
                    .retain(|(offset, _)| *offset < checkpoint.bytes_len);
                if let Some((offset, bytes)) = self.sealed.last_mut() {
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
        self.unicode_indexes
            .truncate(checkpoint.unicode_indexes_len);
        self.len = checkpoint.len;
    }
}

#[cfg(test)]
mod test {
    use std::ops::Deref;

    use super::*;

    #[test]
    fn sealed_prefixes_keep_unicode_slices_and_rollback_branch_local() {
        let mut a = StrArena::default();
        a.alloc("A界🙂Z");
        let mut b = a.fork_cow();
        let checkpoint = b.checkpoint();
        a.alloc("source");
        b.alloc("private");
        assert_eq!(&*a.slice_by_unicode(..), "A界🙂Zsource".as_bytes());
        assert_eq!(&*b.slice_by_unicode(..), "A界🙂Zprivate".as_bytes());
        let mut c = b.fork_cow();
        b.rollback(checkpoint);
        b.alloc("!");
        c.alloc("尾");
        assert_eq!(&*b.slice_by_unicode(1..), "界🙂Z!".as_bytes());
        assert_eq!(&*c.slice_by_unicode(2..), "🙂Zprivate尾".as_bytes());
        assert_eq!(&*a.slice_by_unicode(1..3), "界🙂".as_bytes());
    }

    #[test]
    fn test() {
        let mut arena = StrArena::default();
        arena.alloc("Hello");
        let slice = arena.slice_by_unicode(0..5);
        assert_eq!(slice.deref(), b"Hello");
        arena.alloc("World");
        let slice = arena.slice_by_unicode(5..10);
        assert_eq!(slice.deref(), b"World");
    }

    #[test]
    fn parse_unicode_correctly() {
        let mut arena = StrArena::default();
        arena.alloc("Hello");
        arena.alloc("World");
        arena.alloc("你好");
        arena.alloc("世界");
        let slice = arena.slice_by_unicode(0..4);
        assert_eq!(slice.deref(), b"Hell");

        let slice = arena.slice_by_unicode(4..8);
        assert_eq!(slice.deref(), b"oWor");

        let slice = arena.slice_by_unicode(8..10);
        assert_eq!(slice.deref(), b"ld");

        let slice = arena.slice_by_unicode(10..12);
        assert_eq!(slice.deref(), "你好".as_bytes());

        let slice = arena.slice_by_unicode(12..14);
        assert_eq!(slice.deref(), "世界".as_bytes());
    }

    #[test]
    fn slice_long_unicode_correctly() {
        let mut arena = StrArena::default();
        let src = "一二34567八九零";
        for s in std::iter::repeat_n(src, 100) {
            arena.alloc(s);
        }

        let slice = arena.slice_by_unicode(110..120);
        assert_eq!(slice.deref(), src.as_bytes());
        let slice = arena.slice_by_unicode(111..121);
        assert_eq!(slice.deref(), "二34567八九零一".as_bytes());
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
        assert_eq!(slice.deref(), b"hello!");
    }
}
