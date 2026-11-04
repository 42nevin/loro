use std::{borrow::Cow, fmt::Debug};

use crate::text_slice::TextSlice as BytesSlice;
use generic_btree::rle::{HasLength, Mergeable, Sliceable, TryInsert};
use rle::Mergable;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    container::richtext::richtext_state::{unicode_to_utf8_index, utf16_to_utf8_index},
    delta::DeltaValue,
};

use super::utf16::{count_unicode_chars, count_utf16_len};

#[derive(Clone)]
pub struct StringSlice {
    bytes: Variant,
}

impl PartialEq for StringSlice {
    fn eq(&self, other: &Self) -> bool {
        self.with_str("string_slice.eq", |left| {
            other.with_str("string_slice.eq", |right| left == right)
        })
    }
}

impl Eq for StringSlice {}

#[derive(Clone, PartialEq, Eq)]
enum Variant {
    BytesSlice(BytesSlice),
    Owned(String),
}

impl From<String> for StringSlice {
    fn from(s: String) -> Self {
        Self {
            bytes: Variant::Owned(s),
        }
    }
}

impl From<BytesSlice> for StringSlice {
    fn from(s: BytesSlice) -> Self {
        Self::new(s)
    }
}

impl From<append_only_bytes::BytesSlice> for StringSlice {
    fn from(s: append_only_bytes::BytesSlice) -> Self {
        Self::new(s)
    }
}

impl From<&str> for StringSlice {
    fn from(s: &str) -> Self {
        Self {
            bytes: Variant::Owned(s.to_string()),
        }
    }
}

impl Debug for StringSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with_str("string_slice.debug", |text| {
            f.debug_struct("StringSlice").field("bytes", &text).finish()
        })
    }
}

impl StringSlice {
    pub fn new(s: impl Into<BytesSlice>) -> Self {
        let s = s.into();
        s.with_bytes("string_slice.validate", |bytes| {
            std::str::from_utf8(bytes).unwrap();
        });
        Self {
            bytes: Variant::BytesSlice(s),
        }
    }

    /// Returns a temporary materialization for logical extents, never caches it.
    pub fn as_str(&self) -> Cow<'_, str> {
        match &self.bytes {
            Variant::BytesSlice(s) => s.as_str_for("string_slice.as_str"),
            Variant::Owned(s) => Cow::Borrowed(s),
        }
    }

    pub fn with_str<R>(&self, consumer: &'static str, visit: impl FnOnce(&str) -> R) -> R {
        match &self.bytes {
            Variant::BytesSlice(s) => s.with_str(consumer, visit),
            Variant::Owned(s) => visit(s),
        }
    }

    fn with_bytes<R>(&self, consumer: &'static str, visit: impl FnOnce(&[u8]) -> R) -> R {
        match &self.bytes {
            Variant::BytesSlice(s) => s.with_bytes(consumer, visit),
            Variant::Owned(s) => visit(s.as_bytes()),
        }
    }

    pub fn len_bytes(&self) -> usize {
        match &self.bytes {
            Variant::BytesSlice(s) => s.len(),
            Variant::Owned(s) => s.len(),
        }
    }

    pub fn len_unicode(&self) -> usize {
        self.with_bytes("string_slice.len_unicode", count_unicode_chars)
    }

    pub fn len_utf16(&self) -> usize {
        self.with_bytes("string_slice.len_utf16", count_utf16_len)
    }

    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    pub fn extend(&mut self, s: &str) {
        match &mut self.bytes {
            Variant::BytesSlice(_) => {
                *self = Self {
                    bytes: Variant::Owned(
                        self.with_str("string_slice.extend_owned", |text| format!("{}{}", text, s)),
                    ),
                }
            }
            Variant::Owned(v) => {
                v.push_str(s);
            }
        }
    }
}

impl std::fmt::Display for StringSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with_str("string_slice.display", |text| f.write_str(text))
    }
}

impl<'de> Deserialize<'de> for StringSlice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(s.into())
    }
}

impl Serialize for StringSlice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.with_str("string_slice.serialize", |text| {
            serializer.serialize_str(text)
        })
    }
}

impl DeltaValue for StringSlice {
    fn value_extend(&mut self, other: Self) -> Result<(), Self> {
        match (&mut self.bytes, &other.bytes) {
            (Variant::BytesSlice(s), Variant::BytesSlice(o)) => match s.try_merge(o) {
                Ok(_) => Ok(()),
                Err(_) => Err(other),
            },
            (Variant::Owned(s), _) => {
                other.with_str("string_slice.value_extend", |text| s.push_str(text));
                Ok(())
            }
            _ => Err(other),
        }
    }

    fn take(&mut self, length: usize) -> Self {
        let length = if cfg!(feature = "wasm") {
            self.with_str("string_slice.index", |text| {
                utf16_to_utf8_index(text, length).unwrap()
            })
        } else {
            self.with_str("string_slice.index", |text| {
                unicode_to_utf8_index(text, length).unwrap()
            })
        };

        match &mut self.bytes {
            Variant::BytesSlice(s) => {
                let mut other = s.slice_clone(length..);
                s.slice_(..length);
                std::mem::swap(s, &mut other);
                Self {
                    bytes: Variant::BytesSlice(other),
                }
            }
            Variant::Owned(s) => {
                let mut other = s.split_off(length);
                std::mem::swap(s, &mut other);
                Self {
                    bytes: Variant::Owned(other),
                }
            }
        }
    }

    /// Unicode length of the string
    /// Utf16 length when in WASM
    fn length(&self) -> usize {
        if cfg!(feature = "wasm") {
            self.len_utf16()
        } else {
            self.len_unicode()
        }
    }
}

impl HasLength for StringSlice {
    fn rle_len(&self) -> usize {
        if cfg!(feature = "wasm") {
            self.len_utf16()
        } else {
            self.len_unicode()
        }
    }
}

impl TryInsert for StringSlice {
    fn try_insert(&mut self, pos: usize, elem: Self) -> Result<(), Self>
    where
        Self: Sized,
    {
        match &mut self.bytes {
            Variant::BytesSlice(_) => Err(elem),
            Variant::Owned(s) => {
                if s.capacity() >= s.len() + elem.len_bytes() {
                    let pos = if cfg!(feature = "wasm") {
                        utf16_to_utf8_index(s.as_str(), pos).unwrap()
                    } else {
                        unicode_to_utf8_index(s.as_str(), pos).unwrap()
                    };
                    elem.with_str("string_slice.try_insert", |text| s.insert_str(pos, text));
                    Ok(())
                } else {
                    Err(elem)
                }
            }
        }

        // match (&mut self.bytes, &elem.bytes) {
        //     (Variant::Owned(a), Variant::Owned(b))
        //         // TODO: Extract magic num
        //         if a.capacity() >= a.len() + b.len() && a.capacity() < 128 =>
        //     {
        //         a.insert_str(pos, b.as_str());
        //         Ok(())
        //     }
        //     _ => Err(elem),
        // }
    }
}

impl Mergeable for StringSlice {
    fn can_merge(&self, rhs: &Self) -> bool {
        match (&self.bytes, &rhs.bytes) {
            (Variant::BytesSlice(a), Variant::BytesSlice(b)) => a.can_merge(b),
            (Variant::Owned(a), Variant::Owned(b)) => a.len() + b.len() <= a.capacity(),
            _ => false,
        }
    }

    fn merge_right(&mut self, rhs: &Self) {
        match (&mut self.bytes, &rhs.bytes) {
            (Variant::BytesSlice(a), Variant::BytesSlice(b)) => a.merge(b, &()),
            (Variant::Owned(a), Variant::Owned(b)) => a.push_str(b.as_str()),
            _ => {}
        }
    }

    fn merge_left(&mut self, left: &Self) {
        match (&mut self.bytes, &left.bytes) {
            (Variant::BytesSlice(a), Variant::BytesSlice(b)) => {
                let mut new = b.clone();
                new.merge(a, &());
                *a = new;
            }
            (Variant::Owned(a), Variant::Owned(b)) => {
                a.insert_str(0, b.as_str());
            }
            _ => {}
        }
    }
}

impl Sliceable for StringSlice {
    fn _slice(&self, range: std::ops::Range<usize>) -> Self {
        let range = if cfg!(feature = "wasm") {
            let start = self.with_str("string_slice.index", |text| {
                utf16_to_utf8_index(text, range.start).unwrap()
            });
            let end = self.with_str("string_slice.index", |text| {
                utf16_to_utf8_index(text, range.end).unwrap()
            });
            start..end
        } else {
            let start = self.with_str("string_slice.index", |text| {
                unicode_to_utf8_index(text, range.start).unwrap()
            });
            let end = self.with_str("string_slice.index", |text| {
                unicode_to_utf8_index(text, range.end).unwrap()
            });
            start..end
        };

        let bytes = match &self.bytes {
            Variant::BytesSlice(s) => Variant::BytesSlice(s.slice_clone(range)),
            Variant::Owned(s) => Variant::Owned(s[range].to_string()),
        };

        Self { bytes }
    }

    fn split(&mut self, pos: usize) -> Self {
        let pos = if cfg!(feature = "wasm") {
            self.with_str("string_slice.index", |text| {
                utf16_to_utf8_index(text, pos).unwrap()
            })
        } else {
            self.with_str("string_slice.index", |text| {
                unicode_to_utf8_index(text, pos).unwrap()
            })
        };

        let bytes = match &mut self.bytes {
            Variant::BytesSlice(s) => {
                let other = s.slice_clone(pos..);
                s.slice_(..pos);
                Variant::BytesSlice(other)
            }
            Variant::Owned(s) => {
                let other = s.split_off(pos);
                Variant::Owned(other)
            }
        };

        Self { bytes }
    }
}

impl Default for StringSlice {
    fn default() -> Self {
        StringSlice {
            bytes: Variant::Owned(String::with_capacity(32)),
        }
    }
}

impl loro_delta::delta_trait::DeltaValue for StringSlice {}
#[allow(dead_code)]
pub fn unicode_range_to_byte_range(s: &str, start: usize, end: usize) -> (usize, usize) {
    debug_assert!(start <= end);
    let start_unicode_index = start;
    let end_unicode_index = end;
    let mut current_utf8_index = 0;
    let mut start_byte = 0;
    let mut end_byte = s.len();
    for (current_unicode_index, c) in s.chars().enumerate() {
        if current_unicode_index == start_unicode_index {
            start_byte = current_utf8_index;
        }

        if current_unicode_index == end_unicode_index {
            end_byte = current_utf8_index;
            break;
        }

        current_utf8_index += c.len_utf8();
    }

    (start_byte, end_byte)
}

#[cfg(test)]
mod tests {
    use append_only_bytes::AppendOnlyBytes;
    use generic_btree::rle::{HasLength, Mergeable, Sliceable, TryInsert};

    use crate::delta::DeltaValue;

    use super::*;

    #[test]
    fn logical_extent_splits_read_scoped_without_retaining_materialization() {
        use std::{
            io,
            ops::Range,
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
        };
        struct Resolver {
            reads: AtomicUsize,
            copied: AtomicUsize,
        }
        impl crate::text_slice::ExtentResolver for Resolver {
            fn with_bytes(
                &self,
                extent: u64,
                range: Range<usize>,
                _consumer: &'static str,
                visit: &mut dyn FnMut(&[u8]),
            ) -> io::Result<()> {
                assert_eq!(extent, 17);
                self.reads.fetch_add(1, Ordering::SeqCst);
                visit(&"aé😀b".as_bytes()[range]);
                Ok(())
            }
            fn record_materialization(
                &self,
                _extent: u64,
                _range: Range<usize>,
                _consumer: &'static str,
                bytes: usize,
            ) {
                self.copied.fetch_add(bytes, Ordering::SeqCst);
            }
        }
        let resolver = Arc::new(Resolver {
            reads: AtomicUsize::new(0),
            copied: AtomicUsize::new(0),
        });
        let mut slice = StringSlice::new(BytesSlice::extent(resolver.clone(), 17, 0..8));
        assert_eq!(slice.len_unicode(), 4);
        assert_eq!(slice.len_utf16(), 5);
        let middle = slice._slice(1..3);
        assert!(middle.with_str("test.middle", |s| s == "é😀"));
        let right = slice.split(2);
        assert!(slice.with_str("test.left", |s| s == "aé"));
        assert!(right.with_str("test.right", |s| s == "😀b"));
        for value in [&slice, &middle, &right] {
            assert!(matches!(
                value.bytes,
                Variant::BytesSlice(BytesSlice::Extent { .. })
            ));
        }
        assert_eq!(resolver.copied.load(Ordering::SeqCst), 0);
        assert!(resolver.reads.load(Ordering::SeqCst) > 0);
        // An explicitly requested contiguous view is temporary and counted.
        assert_eq!(middle.as_str(), "é😀");
        assert_eq!(resolver.copied.load(Ordering::SeqCst), 6);
        assert!(matches!(
            middle.bytes,
            Variant::BytesSlice(BytesSlice::Extent { .. })
        ));
    }

    #[test]
    fn owned_slice_lengths_and_unicode_boundaries_use_logical_char_indices() {
        let mut slice = StringSlice::from("aé😀b");
        assert_eq!(slice.as_str(), "aé😀b");
        assert_eq!(slice.len_bytes(), 8);
        assert_eq!(slice.len_unicode(), 4);
        assert_eq!(slice.len_utf16(), 5);
        let event_len = if cfg!(feature = "wasm") { 5 } else { 4 };
        assert_eq!(slice.length(), event_len);
        assert_eq!(slice.rle_len(), event_len);

        let middle_range = if cfg!(feature = "wasm") { 1..4 } else { 1..3 };
        let middle = slice._slice(middle_range);
        assert_eq!(middle.as_str(), "é😀");

        let right = slice.split(2);
        assert_eq!(slice.as_str(), "aé");
        assert_eq!(right.as_str(), "😀b");

        let prefix = slice.take(1);
        assert_eq!(prefix.as_str(), "a");
        assert_eq!(slice.as_str(), "é");
    }

    #[test]
    fn owned_slice_insert_and_merge_respect_capacity_contracts() {
        let mut backing = String::with_capacity(16);
        backing.push_str("a😀");
        let mut slice = StringSlice::from(backing);
        slice.try_insert(1, StringSlice::from("é")).unwrap();
        assert_eq!(slice.as_str(), "aé😀");

        let mut tight = StringSlice::from(String::from("ab"));
        let rejected = tight.try_insert(1, StringSlice::from("c")).unwrap_err();
        assert_eq!(tight.as_str(), "ab");
        assert_eq!(rejected.as_str(), "c");

        let mut left_text = String::with_capacity(8);
        left_text.push_str("ab");
        let mut left = StringSlice::from(left_text);
        let right = StringSlice::from("cd");
        assert!(left.can_merge(&right));
        left.merge_right(&right);
        assert_eq!(left.as_str(), "abcd");

        let mut suffix = StringSlice::from("cd");
        suffix.merge_left(&StringSlice::from("ab"));
        assert_eq!(suffix.as_str(), "abcd");
    }

    #[test]
    fn bytes_slice_variant_keeps_append_only_slice_semantics() {
        let mut bytes = AppendOnlyBytes::new();
        bytes.push_str("left");
        let left_bytes = bytes.slice(..);
        bytes.push_str("right");
        let right_bytes = bytes.slice(4..);

        let mut left = StringSlice::from(left_bytes.clone());
        let right = StringSlice::from(right_bytes.clone());
        assert_eq!(left.as_str(), "left");
        assert_eq!(right.as_str(), "right");
        assert!(left.can_merge(&right));
        left.value_extend(right).unwrap();
        assert_eq!(left.as_str(), "leftright");

        let mut whole = StringSlice::from(bytes.slice(..));
        let tail = whole.split(4);
        assert_eq!(whole.as_str(), "left");
        assert_eq!(tail.as_str(), "right");
        assert_eq!(
            StringSlice::from(bytes.slice(..))._slice(1..5).as_str(),
            "eftr"
        );

        let mut converted_to_owned = StringSlice::from(left_bytes);
        converted_to_owned.extend("!");
        assert_eq!(converted_to_owned.as_str(), "left!");

        let mut byte_slice = StringSlice::from(right_bytes);
        let rejected = byte_slice
            .try_insert(1, StringSlice::from("!"))
            .unwrap_err();
        assert_eq!(byte_slice.as_str(), "right");
        assert_eq!(rejected.as_str(), "!");
    }

    #[test]
    fn serde_display_default_and_range_conversion_are_string_like() {
        let empty = StringSlice::default();
        assert!(empty.is_empty());

        let slice = StringSlice::from("aé😀b");
        assert_eq!(slice.to_string(), "aé😀b");
        assert_eq!(format!("{slice:?}"), r#"StringSlice { bytes: "aé😀b" }"#);

        let encoded = serde_json::to_string(&slice).unwrap();
        assert_eq!(encoded, r#""aé😀b""#);
        let decoded: StringSlice = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, slice);

        let (start, end) = unicode_range_to_byte_range("aé😀b", 1, 3);
        assert_eq!(&"aé😀b"[start..end], "é😀");
        assert_eq!(unicode_range_to_byte_range("aé😀b", 2, 4), (3, 8));
    }
}
