//! Read-only, independently native-decoded reference oracle.
use std::sync::Arc;
use crate::{ChangeMeta, Frontiers, IdSpan, JsonChange, JsonSchema, LoroDoc, LoroResult, LoroValue, VersionVector, ID};
pub use loro_internal::native_reference_oracle::{
    NativeTextLookup, NativeStateProjection, NativeStateText, NativeStateTextRun, NativeStateStyleAnchor,
};
use crate::native_wire_recipe::NativeTextSlice;

/// Eager native proof owner. It intentionally has no editing, importing,
/// exporting, forking, or underlying-document extraction interface.
pub struct NativeReferenceOracle {
    inner: loro_internal::native_reference_oracle::NativeReferenceOracle,
}
impl NativeReferenceOracle {
    /// Independently decode an Update on a fully built, settled reference base.
    pub fn from_update(base: &LoroDoc, wire: &[u8], lookup: Arc<dyn NativeTextLookup>) -> LoroResult<Self> {
        Ok(Self { inner: loro_internal::native_reference_oracle::NativeReferenceOracle::from_update(&base.doc, wire, lookup)? })
    }
    /// Independently decode and eagerly validate a complete native snapshot.
    pub fn from_snapshot(wire: &[u8], lookup: Arc<dyn NativeTextLookup>) -> LoroResult<Self> {
        Ok(Self { inner: loro_internal::native_reference_oracle::NativeReferenceOracle::from_snapshot(wire, lookup)? })
    }
    /// Snapshot construction using an empty document's configuration.
    pub fn from_snapshot_with_config(config: &LoroDoc, wire: &[u8], lookup: Arc<dyn NativeTextLookup>) -> LoroResult<Self> {
        Ok(Self { inner: loro_internal::native_reference_oracle::NativeReferenceOracle::from_snapshot_with_config(&config.doc, wire, lookup)? })
    }
    /// Committed operation version.
    pub fn oplog_vv(&self) -> VersionVector { self.inner.oplog_vv() }
    /// Materialized state version.
    pub fn state_vv(&self) -> VersionVector { self.inner.state_vv() }
    /// Committed operation frontiers.
    pub fn oplog_frontiers(&self) -> Frontiers { self.inner.oplog_frontiers() }
    /// Materialized state frontiers.
    pub fn state_frontiers(&self) -> Frontiers { self.inner.state_frontiers() }
    /// Independently decoded native change metadata.
    pub fn get_change(&self, id: ID) -> Option<ChangeMeta> { self.inner.get_change(id) }
    /// Read typed structural operands plus original text handles over a version interval.
    pub fn history(&self, start: &VersionVector, end: &VersionVector) -> (JsonSchema, Vec<NativeTextSlice>) { self.inner.history(start, end) }
    /// Read typed structural operands plus original text handles over an ID interval.
    pub fn history_in_id_span(&self, span: IdSpan) -> (Vec<JsonChange>, Vec<NativeTextSlice>) { self.inner.history_in_id_span(span) }
    /// Read the structural state with text retained as original slice runs.
    pub fn get_deep_value_with_text_slices(&self) -> LoroResult<NativeStateProjection> {
        self.inner.get_deep_value_with_text_slices()
    }
    /// Eager oracles cannot contain a mutable pending transaction.
    pub fn get_pending_txn_len(&self) -> usize { 0 }
    /// Materialize a user-requested deep value; text output allocation is expected.
    pub fn get_deep_value(&self) -> LoroValue { self.inner.get_deep_value() }
}
