#![cfg(feature = "shared_arena_experiment")]
//! Zero native operations preserve their actual admitted wire spelling.
use loro::native_wire_recipe::{
    BoundTextExtents, NativeWireLedger, capture_native_wire_recipe, reconstruct_native_wire_recipe,
};
use loro::{ContainerID, ExportMode, ID, LoroDoc, LoroError, LoroResult};
use std::cell::Cell;
struct EmptyLedger {
    wire: Vec<u8>,
    receipt: bool,
    semantics: bool,
    checks: Cell<usize>,
}
impl NativeWireLedger for EmptyLedger {
    fn text_extents(
        &self,
        _: ID,
        _: &ContainerID,
        _: u32,
        _: usize,
    ) -> LoroResult<Option<BoundTextExtents>> {
        panic!("zero-op update cannot request text extent")
    }
    fn verify_wire_receipt(&self, wire: &[u8]) -> LoroResult<()> {
        self.checks.set(self.checks.get() + 1);
        if self.receipt && wire == self.wire {
            Ok(())
        } else {
            Err(LoroError::DecodeError("receipt refused".into()))
        }
    }
    fn verify_typed_semantics(&self, wire: &[u8]) -> LoroResult<()> {
        self.checks.set(self.checks.get() + 1);
        if !self.semantics {
            return Err(LoroError::DecodeError("typed refused".into()));
        }
        let empty = LoroDoc::new();
        let result = empty.import(wire)?;
        if result.pending.is_none() && empty.oplog_vv().is_empty() {
            Ok(())
        } else {
            Err(LoroError::DecodeError("not empty typed state".into()))
        }
    }
}
#[test]
fn exported_zero_op_native_recipe_preserves_noncanonical_header_and_requires_both_verifiers() {
    let doc = LoroDoc::new();
    doc.get_text("body").insert(0, "prior").unwrap();
    doc.commit();
    let exported = doc.export(ExportMode::updates(&doc.oplog_vv())).unwrap();
    assert!(
        !exported.is_empty(),
        "zero operations are not raw empty bytes"
    );
    for alternate in [false, true] {
        let mut wire = exported.clone();
        if alternate {
            wire[4..16].fill(0xa5);
        }
        let ledger = EmptyLedger {
            wire: wire.clone(),
            receipt: true,
            semantics: true,
            checks: Cell::new(0),
        };
        let recipe = capture_native_wire_recipe(&wire, &ledger).unwrap();
        assert!(recipe.holes.is_empty());
        assert_eq!(recipe.stats().skeleton_bytes, wire.len());
        assert_eq!(recipe.stats().fallback_bytes, wire.len());
        assert_eq!(
            reconstruct_native_wire_recipe(&recipe, &ledger).unwrap(),
            wire
        );
        assert!(ledger.checks.get() >= 4);
        for receipt in [false, true] {
            let refused = EmptyLedger {
                wire: wire.clone(),
                receipt,
                semantics: !receipt,
                checks: Cell::new(0),
            };
            assert!(capture_native_wire_recipe(&wire, &refused).is_err());
            assert!(reconstruct_native_wire_recipe(&recipe, &refused).is_err());
        }
    }
}
#[test]
fn raw_empty_and_malformed_nonempty_do_not_gain_zero_op_admission() {
    for wire in [vec![], vec![0], vec![0; 22]] {
        let ledger = EmptyLedger {
            wire: wire.clone(),
            receipt: true,
            semantics: true,
            checks: Cell::new(0),
        };
        assert!(capture_native_wire_recipe(&wire, &ledger).is_err());
    }
}
