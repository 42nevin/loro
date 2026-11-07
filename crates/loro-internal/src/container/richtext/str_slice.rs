use crate::text_slice::TextSlice as BytesSlice;
use append_only_bytes::AppendOnlyBytes;
use std::borrow::Cow;

/// It's just a wrapper around [BytesSlice] that makes sure
/// the content of the bytes is a valid utf-8 string
pub(crate) struct StrSlice(BytesSlice);

impl StrSlice {
    #[allow(unused)]
    pub fn new(bytes: BytesSlice) -> Option<Self> {
        bytes
            .with_bytes("str_slice.validate", |text| {
                std::str::from_utf8(text).map(|_| ())
            })
            .ok()?;
        Some(Self(bytes))
    }

    pub fn new_from_str(str: &str) -> Self {
        let mut a = AppendOnlyBytes::new();
        a.push_str(str);
        Self(a.slice(..).into())
    }

    pub fn bytes(&self) -> &BytesSlice {
        &self.0
    }

    pub fn as_str(&self) -> Cow<'_, str> {
        self.0.as_str_for("str_slice.as_str")
    }

    pub fn split_at_unicode_pos(&self, pos: usize) -> (Self, Self) {
        let split_at = if self.0.is_extent() {
            let mut scalars = 0usize;
            let mut bytes = 0usize;
            self.0.try_for_each_char("str_slice.split", |c| {
                if scalars < pos { bytes += c.len_utf8(); }
                scalars += 1;
                Ok(())
            }).expect("validated text slice backing");
            bytes
        } else {
            self.0.with_str("str_slice.split", |s| {
                s.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(s.len())
            })
        };

        (
            Self(self.0.slice_clone(..split_at)),
            Self(self.0.slice_clone(split_at..)),
        )
    }
}

#[cfg(all(test, feature = "shared_arena_experiment"))]
mod reference_tests {
    use super::*;
    use crate::text_slice::ExtentResolver;
    use std::{io, ops::Range, sync::Arc};
    struct Resolver;
    impl ExtentResolver for Resolver {
        fn with_bytes(&self, _: u64, range: Range<usize>, _: &'static str,
            visit: &mut dyn FnMut(&[u8])) -> io::Result<()> {
            visit(&"a玉🙂".as_bytes()[range]); Ok(())
        }
    }
    #[test]
    fn splitting_reference_text_does_not_join_split_scalars() {
        let resolver = Arc::new(Resolver);
        let mut bytes = BytesSlice::extent(resolver.clone(), 1, 0..2);
        bytes.try_merge(&BytesSlice::extent(resolver, 2, 2..8)).unwrap();
        let text = StrSlice(bytes);
        crate::text_slice::reset_text_copy_metrics();
        let (left, right) = text.split_at_unicode_pos(2);
        assert_eq!((left.bytes().len(), right.bytes().len()), (4, 4));
        assert!(crate::text_slice::text_copy_metrics().values()
            .all(|metric| metric.native_bytes == 0 && metric.extent_bytes == 0));
    }
}
