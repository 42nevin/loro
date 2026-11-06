//! Opt-in, thread-local diagnostic evidence from actual native operations.
//! This is not a persistence ledger or an acceptance/publication authority.
use crate::{arena::{SharedArena, TextAllocationContext}, change::Change,
    container::{ContainerID, list::list_op::InnerListOp}, op::{InnerContent, Op},
    text_slice::TextBackingInfo, version::Frontiers};
use loro_common::ID;
use rle::{HasLength, Sliceable};
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub struct ObservedTextOp {
    pub counter: i32,
    pub container_index: u32,
    pub scalar_count: usize,
    pub entity_pos: u32,
    /// Compatibility summary; use backing_parts for physical identity.
    pub backing: TextBackingInfo,
    pub backing_parts: Vec<TextBackingInfo>,
}
#[derive(Clone, Debug)]
pub struct FinalizedTextOp {
    pub id: ID,
    pub container: ContainerID,
    pub op: ObservedTextOp,
    /// Ordered intersections with original successful local insertions. Empty
    /// means this diagnostic session did not witness the original allocation.
    pub original_ranges: Vec<TextBackingInfo>,
}
#[derive(Clone, Debug)]
pub enum NativeTextObservation {
    Insert { context: TextAllocationContext, op: ObservedTextOp },
    Merge { left: ObservedTextOp, right: ObservedTextOp, result: ObservedTextOp },
    Slice { source: ObservedTextOp, scalar_range: std::ops::Range<usize>, result: ObservedTextOp },
    Finalize { id: ID, lamport: u32, deps: Frontiers, timestamp: i64,
        message: Option<String>, native_op_count: usize, text_ops: Vec<FinalizedTextOp> },
}
#[derive(Default)]
struct Trace {
    events: Vec<NativeTextObservation>,
    originals: Vec<(usize, TextAllocationContext, Op)>,
}
thread_local! { static TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) }; }
/// Start/reset evidence on the current thread. Diagnostic-only: retains original
/// insertion handles until `take_native_text_observations` and is outside timings.
pub fn start_native_text_observations() { TRACE.with(|x| *x.borrow_mut() = Some(Trace::default())); }
/// Disable observation and release retained handles, returning ordered events.
pub fn take_native_text_observations() -> Vec<NativeTextObservation> {
    TRACE.with(|x| x.borrow_mut().take().map(|t| t.events).unwrap_or_default())
}
pub(crate) fn enabled() -> bool { TRACE.with(|x| x.borrow().is_some()) }
pub(crate) fn describe(op: &Op) -> Option<ObservedTextOp> {
    let InnerContent::List(InnerListOp::InsertText { slice, pos, .. }) = &op.content else { return None; };
    Some(ObservedTextOp { counter: op.counter, container_index: op.container.to_index(),
        scalar_count: op.atom_len(), entity_pos: *pos, backing: slice.backing_info(), backing_parts: slice.backing_parts() })
}
pub(crate) fn emit(event: NativeTextObservation) {
    TRACE.with(|x| { if let Some(t) = x.borrow_mut().as_mut() { t.events.push(event); } });
}
pub(crate) fn insertion(doc: usize, context: TextAllocationContext, op: &Op) {
    if let Some(observed) = describe(op) {
        TRACE.with(|x| { if let Some(t) = x.borrow_mut().as_mut() {
            // A failed/rolled-back batch can reuse its native ID. The newest
            // successful insertion supersedes earlier tentative evidence.
            t.originals.retain(|(d, c, _)| *d != doc || c.id != context.id);
            t.originals.push((doc, context.clone(), op.clone()));
            t.events.push(NativeTextObservation::Insert { context, op: observed });
        } });
    }
}
pub(crate) fn finalized(doc: usize, arena: &SharedArena, change: &Change) -> NativeTextObservation {
    let text_ops = TRACE.with(|x| {
        let trace = x.borrow();
        change.ops.iter().filter_map(|op| {
            let observed = describe(op)?;
            let mut original_ranges = Vec::new();
            if let Some(t) = trace.as_ref() {
                let mut originals: Vec<_> = t.originals.iter().filter(|(d,c,o)| *d == doc
                    && c.id.peer == change.id.peer && o.container == op.container
                    && o.counter < op.counter + op.atom_len() as i32
                    && o.counter + o.atom_len() as i32 > op.counter).collect();
                originals.sort_by_key(|(_,c,_)| c.id.counter);
                for (_,_,original) in originals {
                    let from = (op.counter - original.counter).max(0) as usize;
                    let to = ((op.counter + op.atom_len() as i32 - original.counter) as usize).min(original.atom_len());
                    // Slice native content directly to preserve scalar/byte
                    // correspondence without emitting another Op-level event.
                    if let InnerContent::List(InnerListOp::InsertText { slice, .. }) = original.content.slice(from, to) {
                        let final_from = (original.counter - op.counter).max(0) as usize;
                        let final_to = final_from + to - from;
                        let InnerContent::List(InnerListOp::InsertText { slice: final_slice, .. }) = op.content.slice(final_from, final_to) else { unreachable!() };
                        if slice.same_backing(&final_slice) { original_ranges.extend(slice.backing_parts()); }
                    }
                }
            }
            Some(FinalizedTextOp { id: ID::new(change.id.peer, op.counter),
                container: arena.idx_to_id(op.container).expect("registered text container"),
                op: observed, original_ranges })
        }).collect()
    });
    NativeTextObservation::Finalize { id: change.id, lamport: change.lamport,
        deps: change.deps.clone(), timestamp: change.timestamp,
        message: change.commit_msg.as_ref().map(|s| s.to_string()),
        native_op_count: change.ops.iter().count(), text_ops }
}
