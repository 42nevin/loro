//! Experimental exact-wire skeleton proof. This is NOT a typed wire AST or a
//! persistence format approved for production. Literal bytes remain receipt
//! authority. The external ledger must independently verify typed semantics and
//! the application's exact receipt, including its own cryptographic hash.
use crate::{
    encoding::{parse_header_and_body, EncodeMode},
    oplog::ChangeStore,
    text_slice::TextSlice,
    LoroDoc,
};
use loro_common::{ContainerID, LoroError, LoroResult, ID};
use serde::{Deserialize, Serialize};
pub use crate::encoding::json_schema::NativeTextSlice;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtentProvenance {
    OriginalLocal,
    ReceivedRemote,
}
#[derive(Clone, Debug)]
pub struct BoundTextExtents {
    pub provenance: ExtentProvenance,
    pub parts: Vec<TextSlice>,
}

impl BoundTextExtents {
    /// Compare original physical ranges with an independent expected operand.
    /// Byte cursors deliberately permit a scalar to cross a physical range;
    /// matching a UTF-8 expected operand validates the complete text, not each
    /// arbitrary storage chunk. No temporary contiguous payload is created.
    pub fn matches_bytes(&self, expected: &[u8]) -> LoroResult<bool> {
        let mut offset = 0usize;
        let mut matches = true;
        for part in &self.parts {
            part.try_for_each_chunk("wire_recipe.compare", |bytes| {
                let end = offset.checked_add(bytes.len()).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "text length overflow")
                })?;
                matches &= expected.get(offset..end) == Some(bytes);
                offset = end;
                Ok(())
            }).map_err(|_| invalid())?;
        }
        Ok(matches && offset == expected.len())
    }
}

/// Trusted application-owned allocation ledger and correspondence verifier.
/// `OriginalLocal` means the original allocation identity, not a content match.
/// `ReceivedRemote` must use receiver-local identities. Verification callbacks
/// must use independent typed records and the external expected receipt; a
/// recipe's own fields cannot establish either authority.
pub trait NativeWireLedger {
    fn text_extents(
        &self,
        op: ID,
        container: &ContainerID,
        unicode_len: u32,
        byte_len: usize,
    ) -> LoroResult<Option<BoundTextExtents>>;
    fn verify_typed_semantics(&self, wire: &[u8]) -> LoroResult<()>;
    fn verify_wire_receipt(&self, wire: &[u8]) -> LoroResult<()>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeExtentRef {
    pub extent: u64,
    pub start: usize,
    pub end: usize,
}
/// Authenticated insertion metadata for a JSON schema with empty text slots.
/// It carries no independent acceptance authority: the caller must bind the
/// schema and these descriptors to its original operation/receipt proof.
#[derive(Clone, Debug)]
pub struct NativeTextReference {
    pub op: ID,
    pub container: ContainerID,
    pub unicode_len: u32,
    pub byte_len: usize,
    pub provenance: ExtentProvenance,
    pub extents: Vec<NativeExtentRef>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTextHole {
    pub offset: usize,
    pub byte_len: usize,
    pub unicode_len: u32,
    pub op: ID,
    pub container: ContainerID,
    pub provenance: ExtentProvenance,
    pub extents: Vec<NativeExtentRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeWireRecipe {
    pub version: u32,
    pub original_wire_bytes: usize,
    pub fragments: Vec<Vec<u8>>,
    pub holes: Vec<NativeTextHole>,
    /// Complete recognized text operands retained literally due to missing ledger bindings.
    pub fallback_text_bytes: usize,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NativeWireRecipeStats {
    pub original_wire_bytes: usize,
    pub skeleton_bytes: usize,
    pub referenced_extent_bytes: usize,
    pub unique_referenced_extent_bytes: usize,
    pub fallback_bytes: usize,
    pub fallback_text_bytes: usize,
    pub metadata_json_bytes: usize,
    /// Full native validation plus annotation decodes per capture/reconstruction.
    pub native_decode_passes: usize,
    pub external_verification_calls: usize,
    /// Reconstructed output buffer bytes, excluding native validation allocations.
    pub reconstruction_copy_bytes: usize,
    pub capture_copy_bytes: usize,
    pub extent_resolution_bytes_per_call: usize,
}
impl NativeWireRecipe {
    pub fn stats(&self) -> NativeWireRecipeStats {
        // Metadata serialization excludes literal payload bytes; Vec headers and
        // allocator/database overhead are not included in this logical measure.
        let metadata_json_bytes = serde_json::to_vec(&(
            self.version,
            self.original_wire_bytes,
            &self.holes,
            self.fallback_text_bytes,
            self.fragments.iter().map(Vec::len).collect::<Vec<_>>(),
        ))
        .map_or(0, |v| v.len());
        let mut extent_ranges = std::collections::BTreeMap::<u64, Vec<(usize, usize)>>::new();
        for extent in self.holes.iter().flat_map(|h| &h.extents) {
            extent_ranges
                .entry(extent.extent)
                .or_default()
                .push((extent.start, extent.end));
        }
        let mut unique_referenced_extent_bytes = 0usize;
        for ranges in extent_ranges.values_mut() {
            ranges.sort_unstable();
            let mut covered_end = 0;
            for &(start, end) in ranges.iter() {
                unique_referenced_extent_bytes = unique_referenced_extent_bytes
                    .saturating_add(end.saturating_sub(start.max(covered_end)));
                covered_end = covered_end.max(end);
            }
        }
        NativeWireRecipeStats {
            original_wire_bytes: self.original_wire_bytes,
            skeleton_bytes: self.fragments.iter().map(Vec::len).sum(),
            referenced_extent_bytes: self
                .holes
                .iter()
                .fold(0usize, |n, h| n.saturating_add(h.byte_len)),
            unique_referenced_extent_bytes,
            fallback_bytes: if self.holes.is_empty() {
                self.original_wire_bytes
            } else {
                0
            },
            fallback_text_bytes: self.fallback_text_bytes,
            metadata_json_bytes,
            native_decode_passes: if self.fragments.first().and_then(|f| f.get(20..22))
                == Some(&[0, 4][..])
            {
                2
            } else {
                1
            },
            external_verification_calls: 2,
            reconstruction_copy_bytes: self.original_wire_bytes,
            capture_copy_bytes: self.original_wire_bytes,
            extent_resolution_bytes_per_call: self
                .holes
                .iter()
                .fold(0usize, |n, h| n.saturating_add(h.byte_len)),
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct NativeTextSpan {
    pub offset: usize,
    pub byte_len: usize,
    pub unicode_len: u32,
    pub op: ID,
    pub container: ContainerID,
}
fn invalid() -> LoroError {
    LoroError::DecodeError("invalid native wire recipe or extent binding".into())
}
fn check_native(wire: &[u8], ledger: &dyn NativeWireLedger,
    lookup: Option<&dyn crate::native_reference_oracle::NativeTextLookup>) -> LoroResult<()> {
    ledger.verify_wire_receipt(wire)?;
    if let Some(lookup) = lookup {
        // This pass checks native grammar just like the existing fresh-doc pass.
        // Delta dependencies may be pending here; the independent application
        // semantic verifier below checks them against the actual base.
        LoroDoc::new().import_reference_updates(wire, lookup)?;
    } else {
        LoroDoc::new_auto_commit().import(wire)?;
    }
    ledger.verify_typed_semantics(wire)
}
fn spans(wire: &[u8], lookup: Option<&dyn crate::native_reference_oracle::NativeTextLookup>) -> LoroResult<Vec<NativeTextSpan>> {
    let parsed = parse_header_and_body(wire, true)?;
    if !matches!(parsed.mode, EncodeMode::FastUpdates) {
        return Ok(Vec::new());
    }
    let mut reader = parsed.body;
    let mut result = Vec::new();
    while !reader.is_empty() {
        let len = usize::try_from(leb128::read::unsigned(&mut reader).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
        if len > reader.len() {
            return Err(invalid());
        }
        let offset = wire.len() - reader.len();
        let spans = match lookup {
            Some(lookup) => ChangeStore::annotate_text_spans_with_lookup(&reader[..len], lookup)?,
            None => ChangeStore::annotate_text_spans(&reader[..len])?,
        };
        for mut span in spans {
            span.offset = span.offset.checked_add(offset).ok_or_else(invalid)?;
            result.push(span);
        }
        reader = &reader[len..];
    }
    Ok(result)
}
pub(crate) fn extent_refs(binding: &BoundTextExtents) -> LoroResult<Vec<NativeExtentRef>> {
    if binding.parts.is_empty() {
        return Err(invalid());
    }
    let mut refs = Vec::new();
    for info in binding.parts.iter().flat_map(TextSlice::backing_parts) {
        let extent = info.extent.ok_or_else(invalid)?;
        if info.range.start >= info.range.end {
            return Err(invalid());
        }
        let item = NativeExtentRef {
            extent,
            start: info.range.start,
            end: info.range.end,
        };
        // Repeated/overlapping allocation ranges are not a proof of original
        // operand coverage. Reject them even when their contents happen to match.
        if refs.iter().any(|r: &NativeExtentRef| {
            r.extent == extent && r.start < item.end && item.start < r.end
        }) {
            return Err(invalid());
        }
        refs.push(item);
    }
    Ok(refs)
}
fn append_binding(binding: &BoundTextExtents, out: &mut Vec<u8>) -> LoroResult<()> {
    for part in &binding.parts {
        let before = out.len();
        let result = part.try_for_each_chunk("wire_recipe.reconstruct", |bytes| {
            out.extend_from_slice(bytes);
            Ok(())
        });
        // The final exact Update is a necessary contiguous output. Charge
        // actual writes even if a resolver subsequently reports an error.
        part.record_materialization("wire_recipe.reconstruct", out.len() - before);
        result.map_err(|_| invalid())?;
    }
    Ok(())
}
/// Capture exact literal syntax plus complete native text-value holes. Missing
/// bindings retain literal bytes; an invalid supplied binding is always an error.
pub fn capture_native_wire_recipe(
    wire: &[u8], ledger: &dyn NativeWireLedger,
) -> LoroResult<NativeWireRecipe> {
    capture_native_wire_recipe_mode(wire, ledger, None)
}
/// Strict reference capture: native parsing and span annotation retain original
/// text owners. Missing lookup/ledger bindings are errors, never literal fallback.
pub fn capture_native_wire_recipe_with_lookup(
    wire: &[u8], ledger: &dyn NativeWireLedger,
    lookup: &dyn crate::native_reference_oracle::NativeTextLookup,
) -> LoroResult<NativeWireRecipe> {
    capture_native_wire_recipe_mode(wire, ledger, Some(lookup))
}
fn capture_native_wire_recipe_mode(
    wire: &[u8], ledger: &dyn NativeWireLedger,
    lookup: Option<&dyn crate::native_reference_oracle::NativeTextLookup>,
) -> LoroResult<NativeWireRecipe> {
    check_native(wire, ledger, lookup)?;
    let mut recipe = NativeWireRecipe {
        version: 1,
        original_wire_bytes: wire.len(),
        fragments: Vec::new(),
        holes: Vec::new(),
        fallback_text_bytes: 0,
    };
    let mut cursor = 0;
    for span in spans(wire, lookup)? {
        let Some(binding) =
            ledger.text_extents(span.op, &span.container, span.unicode_len, span.byte_len)?
        else {
            if lookup.is_some() { return Err(invalid()); }
            recipe.fallback_text_bytes = recipe
                .fallback_text_bytes
                .checked_add(span.byte_len)
                .ok_or_else(invalid)?;
            continue;
        };
        if span.byte_len == 0 || span.offset < cursor {
            return Err(invalid());
        }
        let end = span.offset.checked_add(span.byte_len).ok_or_else(invalid)?;
        let expected = wire.get(span.offset..end).ok_or_else(invalid)?;
        let refs = extent_refs(&binding)?;
        if refs
            .iter()
            .try_fold(0usize, |n, r| n.checked_add(r.end - r.start))
            != Some(span.byte_len)
        {
            return Err(invalid());
        }
        if !binding.matches_bytes(expected)? {
            return Err(invalid());
        }
        recipe.fragments.push(wire[cursor..span.offset].to_vec());
        recipe.holes.push(NativeTextHole {
            offset: span.offset,
            byte_len: span.byte_len,
            unicode_len: span.unicode_len,
            op: span.op,
            container: span.container,
            provenance: binding.provenance,
            extents: refs,
        });
        cursor = end;
    }
    recipe.fragments.push(wire[cursor..].to_vec());
    Ok(recipe)
}
/// Reassemble and verify against independent external receipt/semantic authority.
/// The recipe alone is deliberately insufficient to authorize reconstruction.
pub fn reconstruct_native_wire_recipe(
    recipe: &NativeWireRecipe, ledger: &dyn NativeWireLedger,
) -> LoroResult<Vec<u8>> {
    reconstruct_native_wire_recipe_mode(recipe, ledger, None)
}
/// Reconstruct exact output with independent native reference validation.
pub fn reconstruct_native_wire_recipe_with_lookup(
    recipe: &NativeWireRecipe, ledger: &dyn NativeWireLedger,
    lookup: &dyn crate::native_reference_oracle::NativeTextLookup,
) -> LoroResult<Vec<u8>> {
    reconstruct_native_wire_recipe_mode(recipe, ledger, Some(lookup))
}
fn reconstruct_native_wire_recipe_mode(
    recipe: &NativeWireRecipe, ledger: &dyn NativeWireLedger,
    lookup: Option<&dyn crate::native_reference_oracle::NativeTextLookup>,
) -> LoroResult<Vec<u8>> {
    if lookup.is_some() && recipe.fallback_text_bytes != 0 { return Err(invalid()); }
    if recipe.version != 1
        || recipe.fragments.len() != recipe.holes.len().checked_add(1).ok_or_else(invalid)?
    {
        return Err(invalid());
    }
    let literal_len = recipe
        .fragments
        .iter()
        .try_fold(0usize, |n, f| n.checked_add(f.len()))
        .ok_or_else(invalid)?;
    let total_len = recipe
        .holes
        .iter()
        .try_fold(literal_len, |n, h| n.checked_add(h.byte_len))
        .ok_or_else(invalid)?;
    if total_len != recipe.original_wire_bytes {
        return Err(invalid());
    }
    let mut wire = Vec::with_capacity(total_len);
    for (index, hole) in recipe.holes.iter().enumerate() {
        wire.extend_from_slice(&recipe.fragments[index]);
        if wire.len() != hole.offset || hole.byte_len == 0 {
            return Err(invalid());
        }
        let binding = ledger
            .text_extents(hole.op, &hole.container, hole.unicode_len, hole.byte_len)?
            .ok_or_else(invalid)?;
        if binding.provenance != hole.provenance || extent_refs(&binding)? != hole.extents {
            return Err(invalid());
        }
        if hole.extents.iter().try_fold(0usize, |n, r| {
            r.end
                .checked_sub(r.start)
                .and_then(|len| n.checked_add(len))
        }) != Some(hole.byte_len)
        {
            return Err(invalid());
        }
        append_binding(&binding, &mut wire)?;
        if wire.len() != hole.offset.checked_add(hole.byte_len).ok_or_else(invalid)? {
            return Err(invalid());
        }
    }
    wire.extend_from_slice(recipe.fragments.last().ok_or_else(invalid)?);
    if wire.len() != recipe.original_wire_bytes {
        return Err(invalid());
    }
    check_native(&wire, ledger, lookup)?;
    let spans = spans(&wire, lookup)?;
    let mut fallback_text_bytes = 0usize;
    for span in &spans {
        if !recipe.holes.iter().any(|h| {
            h.offset == span.offset
                && h.byte_len == span.byte_len
                && h.unicode_len == span.unicode_len
                && h.op == span.op
                && h.container == span.container
        }) {
            // A claimed usable binding must not silently disappear into a
            // literal fallback after recipe tampering.
            if ledger
                .text_extents(span.op, &span.container, span.unicode_len, span.byte_len)?
                .is_some()
            {
                return Err(invalid());
            }
            fallback_text_bytes = fallback_text_bytes
                .checked_add(span.byte_len)
                .ok_or_else(invalid)?;
        }
    }
    for hole in &recipe.holes {
        if !spans.iter().any(|s| {
            s.offset == hole.offset
                && s.byte_len == hole.byte_len
                && s.unicode_len == hole.unicode_len
                && s.op == hole.op
                && s.container == hole.container
        }) {
            return Err(invalid());
        }
    }
    if fallback_text_bytes != recipe.fallback_text_bytes {
        return Err(invalid());
    }
    Ok(wire)
}
