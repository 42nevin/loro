//! Existing JSON structural/style grammar with authenticated text correspondence.
//! Full schema/receipt authentication belongs to the caller, before this boundary.
use crate::{
    arena::SharedArena,
    change::Change,
    encoding::json_schema::{decode_changes_with_text_allocator, json::JsonSchema},
    native_wire_recipe::{extent_refs, NativeWireLedger},
};
use loro_common::{LoroError, LoroResult};

fn invalid(message: &str) -> LoroError {
    LoroError::DecodeError(format!("invalid bound text import: {message}").into())
}

pub(crate) fn decode(
    json: JsonSchema,
    arena: &SharedArena,
    ledger: &dyn NativeWireLedger,
) -> LoroResult<Vec<Change>> {
    decode_changes_with_text_allocator(json, arena, |id, container, text| {
        // External callbacks run only inside the existing decode rollback scope.
        // Convert their decode-time panic to Err so earlier allocations roll back.
        // After successful admission the immutable resolver contract still applies:
        // later disappearance/mutation is an invariant failure, not new input.
        let slice = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let unicode_len = u32::try_from(text.chars().count())
                .map_err(|_| invalid("Unicode length overflow"))?;
            let binding = ledger
                .text_extents(id, container, unicode_len, text.len())?
                .ok_or_else(|| invalid("missing insertion binding"))?;
            let refs = extent_refs(&binding)?;
            if refs
                .iter()
                .try_fold(0usize, |n, r| n.checked_add(r.end - r.start))
                != Some(text.len())
            {
                return Err(invalid("incomplete insertion coverage"));
            }
            let mut offset = 0usize;
            for part in &binding.parts {
                let end = offset
                    .checked_add(part.len())
                    .ok_or_else(|| invalid("byte length overflow"))?;
                let expected = text
                    .as_bytes()
                    .get(offset..end)
                    .ok_or_else(|| invalid("part exceeds insertion"))?;
                // Single physical parts compare in place. A supplied composite
                // uses TextSlice's counted temporary join; no verification Vec
                // or payload cache is introduced by this importer.
                let matches = part
                    .try_with_bytes("json.bound_text.verify", |bytes| bytes == expected)
                    .map_err(|_| invalid("extent resolution failed"))?;
                if !matches {
                    return Err(invalid("text differs from authenticated schema"));
                }
                offset = end;
            }
            if offset != text.len() {
                return Err(invalid("incomplete insertion bytes"));
            }
            let mut parts = binding.parts.into_iter();
            let mut slice = parts.next().ok_or_else(|| invalid("empty binding"))?;
            for part in parts {
                slice
                    .try_merge(&part)
                    .map_err(|_| invalid("parts have different resolvers"))?;
            }
            Ok(slice)
        }))
        .unwrap_or_else(|_| Err(invalid("ledger or extent callback panicked")))?;
        let allocated = arena.alloc_verified_text_slice(slice.clone(), text);
        Ok((slice, allocated))
    })
}
