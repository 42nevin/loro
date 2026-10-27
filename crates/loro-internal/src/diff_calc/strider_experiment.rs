//! Local-only, deliberately bounded OT-before-CRDT experiment.
//!
//! Inspired by eg-strider `transform.rs` at e3b80d5e5f88c86121013ba72ef8d974c786f795.
//! This is not that engine or its ordering semantics. Only two strictly separated
//! edits of a single original plain-text run qualify. All other graphs use Loro's
//! unchanged replay. No history, native IDs, wire format or public Loro API changes.

use std::cell::Cell;

use loro_common::{ContainerType, HasCounterSpan, IdFull, ID};
use loro_delta::DeltaRope;
use rle::HasLength;

use super::{DiffMode, InternalContainerDiff, InternalDiff};
use crate::{
    container::{list::list_op::InnerListOp, richtext::richtext_state::RichtextStateChunk},
    op::{InnerContent, Op},
    oplog::{BlockChangeRef, OpLog},
    version::Frontiers,
    VersionVector,
};

/// Experimental diagnostics, scoped to the calling thread (not production config).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub attempts: u64,
    pub hits: u64,
    pub shape_fallbacks: u64,
    pub overlap_fallbacks: u64,
    pub unparsed_fallbacks: u64,
    pub borrowed_base_attempts: u64,
    pub borrowed_base_proofs: u64,
}

thread_local! {
    static BORROWED_BASE: Cell<bool> = const { Cell::new(false) };
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    static STATS: Cell<Stats> = Cell::new(Stats::default());
}

/// Return the previous setting; callers must restore it after differential runs.
pub fn set_enabled(enabled: bool) -> bool {
    ENABLED.with(|value| value.replace(enabled))
}

/// Diagnostic-only alternate proof strategy; parsed-only remains the default.
pub fn set_borrowed_base(enabled: bool) -> bool {
    BORROWED_BASE.with(|value| value.replace(enabled))
}

pub fn reset_stats() {
    STATS.with(|value| value.set(Stats::default()));
}

pub fn stats() -> Stats {
    STATS.with(Cell::get)
}

enum Fallback {
    Unparsed,
    Shape,
    Overlap,
}

pub(super) fn try_disjoint_import(
    oplog: &OpLog,
    before: &VersionVector,
    before_frontiers: &Frontiers,
    after: &VersionVector,
    after_frontiers: &Frontiers,
) -> Option<InternalContainerDiff> {
    if !ENABLED.with(Cell::get) {
        return None;
    }
    let result = calculate(oplog, before, before_frontiers, after, after_frontiers);
    STATS.with(|value| {
        let mut stats = value.get();
        stats.attempts += 1;
        match &result {
            Ok(_) => stats.hits += 1,
            Err(Fallback::Unparsed) => stats.unparsed_fallbacks += 1,
            Err(Fallback::Shape) => stats.shape_fallbacks += 1,
            Err(Fallback::Overlap) => stats.overlap_fallbacks += 1,
        }
        value.set(stats);
    });
    result.ok()
}

/// A whole change, from counter zero, containing exactly one complete op run.
fn whole_run(oplog: &OpLog, tip: ID, decode_branch: bool) -> Result<BlockChangeRef, Fallback> {
    // The parsed-only strategy never decodes. The separate borrowed-base
    // experiment permits ordinary branch decoding, but never base decoding here.
    let change = if decode_branch { oplog.get_change_at(tip) } else { oplog.peek_parsed_change_at(tip) }
        .ok_or(Fallback::Unparsed)?;
    if change.id.counter != 0
        || change.ctr_last() != tip.counter
        || change.ops().vec().len() != 1
        || change.ops().vec()[0].counter != 0
        || change.ops().vec()[0].ctr_end() != change.ctr_end()
    {
        return Err(Fallback::Shape);
    }
    Ok(change)
}

/// Closed edited interval in the shared base; inserts occupy a zero-width gap.
/// Deliberately reject even endpoint contact to avoid Fugue tie/origin decisions.
fn edit_interval(op: &Op, base_peer: u64, base_len: usize) -> Option<(usize, usize, i64)> {
    match &op.content {
        InnerContent::List(InnerListOp::InsertText {
            pos, unicode_len, ..
        }) => {
            let pos = *pos as usize;
            (pos <= base_len && *unicode_len > 0).then_some((pos, pos, i64::from(*unicode_len)))
        }
        InnerContent::List(InnerListOp::Delete(del)) => {
            let start = usize::try_from(del.start()).ok()?;
            let end = usize::try_from(del.end()).ok()?;
            // The sole base run has contiguous native IDs. This holds for reverse
            // deletes too: id_start denotes the leftmost/lowest-counter target.
            (start < end && end <= base_len && del.id_start == ID::new(base_peer, start as i32))
                .then_some((start, end, -((end - start) as i64)))
        }
        _ => None,
    }
}

fn calculate(
    oplog: &OpLog,
    before: &VersionVector,
    before_frontiers: &Frontiers,
    after: &VersionVector,
    after_frontiers: &Frontiers,
) -> Result<InternalContainerDiff, Fallback> {
    let shape = || Fallback::Shape;
    // This shape bounds the proof and work: one base peer and two fresh branch
    // peers, no implicit same-peer predecessor outside the explicit dependencies.
    if !oplog.shallow_since_vv().is_empty()
        || before.len() != 2
        || after.len() != 3
        || after_frontiers.len() != 2
        || !after.includes_vv(before)
    {
        return Err(shape());
    }
    let local_tip = before_frontiers.as_single().ok_or_else(shape)?;
    let remote_tip = after_frontiers
        .iter()
        .find(|id| !before.includes_id(*id))
        .ok_or_else(shape)?;
    if !after_frontiers.contains(&local_tip) || before.contains_key(&remote_tip.peer) {
        return Err(shape());
    }
    let borrowed = BORROWED_BASE.with(Cell::get);
    let local = whole_run(oplog, local_tip, borrowed)?;
    let remote = whole_run(oplog, remote_tip, borrowed)?;
    let base_tip = local.deps().as_single().ok_or_else(shape)?;
    if remote.deps().as_single() != Some(base_tip)
        || base_tip.peer == local_tip.peer
        || base_tip.peer == remote_tip.peer
        || local_tip.peer == remote_tip.peer
        || before.get(&base_tip.peer) != Some(&(base_tip.counter + 1))
        || before.get(&local_tip.peer) != Some(&(local_tip.counter + 1))
        || after.get(&base_tip.peer) != before.get(&base_tip.peer)
        || after.get(&local_tip.peer) != before.get(&local_tip.peer)
        || after.get(&remote_tip.peer) != Some(&(remote_tip.counter + 1))
    {
        return Err(shape());
    }
    let local_op = &local.ops().vec()[0];
    let remote_op = &remote.ops().vec()[0];
    let idx = local_op.container;
    let container_id = oplog.arena.idx_to_id(idx).ok_or_else(shape)?;
    if idx.get_type() != ContainerType::Text || !container_id.is_root()
        || container_id.is_mergeable() || remote_op.container != idx {
        return Err(shape());
    }
    let base_len = match whole_run(oplog, base_tip, false) {
        Ok(base) => {
            let op = &base.ops().vec()[0];
            if !base.deps().is_empty() || op.container != idx { return Err(shape()); }
            let InnerContent::List(InnerListOp::InsertText { pos: 0, unicode_len, .. }) = &op.content
                else { return Err(shape()); };
            let len = *unicode_len as usize;
            if len == 0 || len != op.atom_len() || len != (base_tip.counter + 1) as usize {
                return Err(shape());
            }
            len
        }
        Err(Fallback::Unparsed) if borrowed => {
            STATS.with(|cell| { let mut s = cell.get(); s.borrowed_base_attempts += 1; cell.set(s); });
            // An unsupported or invalid proof never produces a fast delta: the
            // original replay/error path remains responsible for the import.
            let len = oplog.prove_encoded_initial_text(base_tip, &container_id)
                .ok().flatten().ok_or_else(shape)?;
            STATS.with(|cell| { let mut s = cell.get(); s.borrowed_base_proofs += 1; cell.set(s); });
            len
        }
        Err(reason) => return Err(reason),
    };
    let (local_start, local_end, local_growth) =
        edit_interval(local_op, base_tip.peer, base_len).ok_or_else(shape)?;
    let (remote_start, remote_end, _) =
        edit_interval(remote_op, base_tip.peer, base_len).ok_or_else(shape)?;
    let transformed = if remote_end < local_start {
        remote_start as i64
    } else if local_end < remote_start {
        remote_start as i64 + local_growth
    } else {
        return Err(Fallback::Overlap);
    };
    let transformed = usize::try_from(transformed).map_err(|_| shape())?;
    let mut delta = DeltaRope::new();
    match &remote_op.content {
        InnerContent::List(InnerListOp::InsertText {
            unicode_start,
            unicode_len,
            ..
        }) => {
            let end = unicode_start.checked_add(*unicode_len).ok_or_else(shape)?;
            let text = oplog
                .arena
                .slice_by_unicode(*unicode_start as usize..end as usize);
            delta.insert_value(
                transformed,
                RichtextStateChunk::new_text(
                    text,
                    IdFull::new(remote.peer(), remote_op.counter, remote.lamport()),
                ),
                (),
            );
        }
        InnerContent::List(InnerListOp::Delete(del)) => delta.delete(transformed, del.atom_len()),
        _ => return Err(shape()),
    }
    Ok(InternalContainerDiff {
        idx,
        bring_back: false,
        diff: InternalDiff::RichtextRaw(delta).into(),
        // This is an already-resolved state patch, not untransformed linear ops.
        diff_mode: DiffMode::Checkout,
        kept: None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::DiffCalculator;

    #[test]
    fn only_import_constructor_has_strider_capability() {
        assert!(DiffCalculator::new_for_import().allow_strider);
        assert!(!DiffCalculator::new(false).allow_strider);
        assert!(!DiffCalculator::new(true).allow_strider);
    }
}
