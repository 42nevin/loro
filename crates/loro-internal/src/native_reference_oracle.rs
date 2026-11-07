//! Eager, read-only native correspondence oracle. Native bytes, never typed
//! schema, determine all operations. Lookup supplies only immutable storage.
use std::sync::Arc;
pub use crate::state::reference_state::{
    NativeStateProjection, NativeStateText, NativeStateTextRun, NativeStateStyleAnchor,
};
use loro_common::{ContainerID, IdSpan, LoroError, LoroResult, LoroValue, ID};
use crate::{
    encoding::{json_schema::json::{JsonChange, JsonSchema}, parse_header_and_body, EncodeMode},
    native_wire_recipe::{extent_refs, BoundTextExtents, NativeTextSlice},
    text_slice::TextSlice,
    version::Frontiers,
    ChangeMeta, LoroDoc, VersionVector,
};

/// Owned immutable original-text lookup, without receipt or semantic callbacks.
/// Implementations must not reenter the document or retain bounded byte views.
pub trait NativeTextLookup: Send + Sync {
    fn text_extents(&self, op: ID, container: &ContainerID, unicode_len: u32, byte_len: usize)
        -> LoroResult<Option<BoundTextExtents>>;
}

pub(crate) fn invalid(message: &str) -> LoroError {
    LoroError::DecodeError(format!("native reference oracle: {message}").into())
}

/// Bind independently decoded native text before adopting its original owner.
pub(crate) fn bind_native_text(
    lookup: &dyn NativeTextLookup, id: ID, container: &ContainerID, native: &str,
) -> LoroResult<TextSlice> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let unicode_len = u32::try_from(native.chars().count()).map_err(|_| invalid("text length overflow"))?;
        let binding = lookup.text_extents(id, container, unicode_len, native.len())?
            .ok_or_else(|| invalid("missing original text binding"))?;
        let refs = extent_refs(&binding)?;
        if refs.iter().try_fold(0usize, |n, r| n.checked_add(r.end - r.start)) != Some(native.len())
            || !binding.matches_bytes(native.as_bytes())? {
            return Err(invalid("native text differs from original binding"));
        }
        let mut parts = binding.parts.into_iter();
        let mut slice = parts.next().ok_or_else(|| invalid("empty original text binding"))?;
        for part in parts {
            slice.try_merge(&part).map_err(|_| invalid("incompatible original text owners"))?;
        }
        Ok(slice)
    })).unwrap_or_else(|_| Err(invalid("original text lookup panicked")))
}

/// A fully built oracle with no mutation or document-extraction interface.
/// Construction is fallible; successful reads cannot trigger unbound decoding.
pub struct NativeReferenceOracle { doc: LoroDoc }
impl NativeReferenceOracle {
    /// The base must already be fully built. It is isolated through native COW;
    /// incoming bytes are decoded independently with exact native semantics.
    pub fn from_update(base: &LoroDoc, wire: &[u8], lookup: Arc<dyn NativeTextLookup>) -> LoroResult<Self> {
        Self::from_update_lookup(base, wire, lookup.as_ref())
    }
    pub(crate) fn from_update_lookup(base: &LoroDoc, wire: &[u8], lookup: &dyn NativeTextLookup) -> LoroResult<Self> {
        base.check_reference_oracle_base()?;
        let doc = base.fork_cow()?;
        let status = doc.import_reference_updates(wire, lookup)?;
        if status.pending.is_some() || doc.state_vv() != doc.oplog_vv() {
            return Err(invalid("unresolved native dependencies"));
        }
        doc.check_reference_oracle_base()?;
        Ok(Self { doc })
    }
    pub fn from_snapshot(wire: &[u8], lookup: Arc<dyn NativeTextLookup>) -> LoroResult<Self> {
        Self::from_snapshot_with_config(&LoroDoc::new(), wire, lookup)
    }
    pub fn from_snapshot_with_config(config: &LoroDoc, wire: &[u8], lookup: Arc<dyn NativeTextLookup>) -> LoroResult<Self> {
        if config.get_pending_txn_len() != 0 || !config.oplog_vv().is_empty() || !config.state_vv().is_empty() {
            return Err(invalid("snapshot configuration document must be empty"));
        }
        let parsed = parse_header_and_body(wire, true)?;
        if !matches!(parsed.mode, EncodeMode::FastSnapshot) { return Err(invalid("expected native snapshot")); }
        let doc = config.fork_cow()?;
        doc.prepare_reference_oracle()?;
        // Existing snapshot transport currently needs an owned encoded buffer.
        crate::text_slice::record_copy("oracle.snapshot.encoded_body", parsed.body.len(), false);
        // fork_cow inherits auto-commit and opens an empty private transaction.
        // Use the ordinary import barrier on that private doc, never commit the
        // caller's draft. The pre-fork guard above rejects real pending edits.
        // Any late resolver panic invalidates this private construction only.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| doc.with_barrier(|| {
            crate::encoding::fast_snapshot::decode_snapshot_reference(
                &doc, bytes::Bytes::copy_from_slice(parsed.body), Default::default(), lookup.as_ref())
        }))).map_err(|_| invalid("native snapshot reference construction panicked"))??;
        if doc.state_vv() != doc.oplog_vv() { return Err(invalid("unsettled native snapshot")); }
        doc.check_reference_oracle_base()?;
        Ok(Self { doc })
    }
    pub fn oplog_vv(&self) -> VersionVector { self.doc.oplog_vv() }
    pub fn state_vv(&self) -> VersionVector { self.doc.state_vv() }
    pub fn oplog_frontiers(&self) -> Frontiers { self.doc.oplog_frontiers() }
    pub fn state_frontiers(&self) -> Frontiers { self.doc.state_frontiers() }
    pub fn get_change(&self, id: ID) -> Option<ChangeMeta> {
        self.doc.oplog().lock().get_change_at(id).map(|c| ChangeMeta::from_change(&c))
    }
    pub fn history(&self, start: &VersionVector, end: &VersionVector) -> (JsonSchema, Vec<NativeTextSlice>) {
        self.doc.export_json_updates_with_text_slices(start, end)
    }
    pub fn history_in_id_span(&self, span: IdSpan) -> (Vec<JsonChange>, Vec<NativeTextSlice>) {
        self.doc.export_json_in_id_span_with_text_slices(span)
    }
    pub fn get_deep_value_with_text_slices(&self) -> LoroResult<NativeStateProjection> {
        self.doc.get_deep_value_with_text_slices()
    }
    pub fn get_pending_txn_len(&self) -> usize { 0 }
    pub fn get_deep_value(&self) -> LoroValue { self.doc.get_deep_value() }
}
