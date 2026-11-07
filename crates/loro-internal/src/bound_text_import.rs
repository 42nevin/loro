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
            if !binding.matches_bytes(text.as_bytes())? {
                return Err(invalid("text differs from authenticated schema"));
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

/// Reference-only counterpart. Empty text is a placeholder only when an exact
/// descriptor binds that operation; it is never an implicit fallback operand.
pub(crate) fn decode_references(
    json: JsonSchema,
    arena: &SharedArena,
    references: &[crate::native_wire_recipe::NativeTextReference],
    ledger: &dyn NativeWireLedger,
) -> LoroResult<Vec<Change>> {
    use crate::encoding::json_schema::{decode_changes_with_referenced_text, json::{JsonOpContent, TextOp}};
    use std::collections::{BTreeMap, BTreeSet};
    if json.peers.is_some() { return Err(invalid("reference schema must use uncompressed peers")); }
    let mut by_id = BTreeMap::new();
    let mut lengths = BTreeMap::new();
    for reference in references {
        if reference.unicode_len == 0 || reference.byte_len == 0 || by_id.insert(reference.op, reference).is_some() {
            return Err(invalid("empty or duplicate text reference"));
        }
        lengths.insert(reference.op, reference.unicode_len as usize);
    }
    let mut seen = BTreeSet::new();
    for change in &json.changes {
        for op in &change.ops {
            let id = loro_common::ID::new(change.id.peer, op.counter);
            if let JsonOpContent::Text(TextOp::Insert { text, .. }) = &op.content {
                let reference = by_id.get(&id).ok_or_else(|| invalid("missing text reference"))?;
                if !text.is_empty() || reference.container != op.container || !seen.insert(id) {
                    return Err(invalid("reference slot mismatch"));
                }
            } else if by_id.contains_key(&id) {
                return Err(invalid("reference targets non-text operation"));
            }
        }
    }
    if seen.len() != references.len() { return Err(invalid("unused text reference")); }
    decode_changes_with_referenced_text(json, arena, &lengths, |id, container, _| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let reference = by_id.get(&id).ok_or_else(|| invalid("missing text reference"))?;
            if &reference.container != container { return Err(invalid("reference container mismatch")); }
            let binding = ledger.text_extents(id, container, reference.unicode_len, reference.byte_len)?
                .ok_or_else(|| invalid("missing insertion binding"))?;
            let refs = extent_refs(&binding)?;
            if binding.provenance != reference.provenance || refs != reference.extents
                || refs.iter().try_fold(0usize, |n, r| n.checked_add(r.end - r.start)) != Some(reference.byte_len) {
                return Err(invalid("reference extent mismatch"));
            }
            let mut parts = binding.parts.into_iter();
            let mut slice = parts.next().ok_or_else(|| invalid("empty binding"))?;
            for part in parts {
                slice.try_merge(&part).map_err(|_| invalid("parts have different resolvers"))?;
            }
            let allocated = arena.alloc_referenced_text_slice(slice.clone(), reference.unicode_len)
                .map_err(|_| invalid("invalid referenced text"))?;
            Ok((slice, allocated))
        })).unwrap_or_else(|_| Err(invalid("ledger or extent callback panicked")))
    })
}
