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
        let split_at = self.0.with_str("str_slice.split", |s| {
            s.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(s.len())
        });

        (
            Self(self.0.slice_clone(..split_at)),
            Self(self.0.slice_clone(split_at..)),
        )
    }
}
