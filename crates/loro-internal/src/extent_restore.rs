//! Typed text-history recovery from immutable extent descriptors.
//!
//! Descriptors carry original native causal metadata. The importer constructs
//! native `Change<Op>` values directly, adopting each `TextSlice` into the arena;
//! it never reconstructs JSON text payloads or a native wire snapshot first.
use crate::{
    change::Change,
    container::list::list_op::{DeleteSpan, DeleteSpanWithId, InnerListOp},
    op::{InnerContent, Op},
    text_slice::TextSlice,
    version::Frontiers,
};
use loro_common::{ContainerID, ContainerType, LoroError, LoroResult, ID};
use rle::{HasLength, RleVec};
use std::collections::BTreeMap;

/// One original native Change. Physical extent boundaries do not change its ID,
/// causal dependencies, Lamport clock, timestamp or message.
#[derive(Clone, Debug)]
pub struct ExtentChangeDescriptor {
    pub id: ID,
    pub lamport: u32,
    pub timestamp: i64,
    pub deps: Frontiers,
    pub message: Option<String>,
    pub ops: Vec<ExtentOpDescriptor>,
}

/// One typed text op, retaining its original counter and container identity.
#[derive(Clone, Debug)]
pub struct ExtentOpDescriptor {
    pub counter: i32,
    pub container: ContainerID,
    pub content: ExtentTextOp,
}

/// Style-free text insertion/deletion qualification. Insert payloads may be
/// native owned slices (C-copy) or storage-resolved logical slices (C-ref).
#[derive(Clone, Debug)]
pub enum ExtentTextOp {
    Insert {
        pos: u32,
        text: TextSlice,
    },
    /// `len` is signed, matching native deletion direction and atom coverage.
    Delete {
        pos: i32,
        len: i32,
        start_id: ID,
    },
}

fn invalid(message: impl Into<Box<str>>) -> LoroError {
    LoroError::DecodeError(message.into())
}

fn check_id(id: ID, kind: &str) -> LoroResult<()> {
    if id.counter < 0 {
        return Err(invalid(format!(
            "invalid extent descriptor: negative {kind} counter"
        )));
    }
    Ok(())
}

pub(crate) fn decode_extent_changes(
    descriptors: Vec<ExtentChangeDescriptor>,
    oplog: &crate::oplog::OpLog,
) -> LoroResult<Vec<Change>> {
    let arena = &oplog.arena;
    let mut changes = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        check_id(descriptor.id, "change")?;
        if descriptor.ops.is_empty() {
            return Err(invalid("invalid extent descriptor: empty change"));
        }
        for dep in descriptor.deps.iter() {
            check_id(dep, "dependency")?;
        }
        let mut expected_counter = descriptor.id.counter;
        let mut ops: RleVec<[Op; 1]> = RleVec::new();
        for op in descriptor.ops {
            if op.counter != expected_counter {
                return Err(invalid(
                    "invalid extent descriptor: noncontiguous op counters",
                ));
            }
            if op.container.container_type() != ContainerType::Text {
                return Err(invalid("extent descriptors support only text containers"));
            }
            if let ContainerID::Normal { counter, .. } = &op.container {
                if *counter < 0 {
                    return Err(invalid(
                        "invalid extent descriptor: negative container counter",
                    ));
                }
            }
            let container = arena.register_container(&op.container);
            let (content, atom_len) = match op.content {
                ExtentTextOp::Insert { pos, text } => {
                    let info = arena
                        .alloc_text_slice(text.clone())
                        .map_err(|e| invalid(format!("invalid extent insertion: {e}")))?;
                    let count = info
                        .end
                        .checked_sub(info.start)
                        .ok_or_else(|| invalid("invalid extent Unicode range"))?;
                    let unicode_start = u32::try_from(info.start)
                        .map_err(|_| invalid("extent arena Unicode offset overflow"))?;
                    let unicode_len = u32::try_from(count)
                        .map_err(|_| invalid("extent Unicode length overflow"))?;
                    (
                        InnerListOp::InsertText {
                            slice: text,
                            unicode_start,
                            unicode_len,
                            pos,
                        },
                        count,
                    )
                }
                ExtentTextOp::Delete { pos, len, start_id } => {
                    check_id(start_id, "delete start")?;
                    let count = len.unsigned_abs() as usize;
                    (
                        InnerListOp::Delete(DeleteSpanWithId {
                            id_start: start_id,
                            span: DeleteSpan {
                                pos: pos as isize,
                                signed_len: len as isize,
                            },
                        }),
                        count,
                    )
                }
            };
            if atom_len == 0 {
                return Err(invalid("invalid extent descriptor: zero-length operation"));
            }
            content.check_positions()?;
            let count = i32::try_from(atom_len)
                .map_err(|_| invalid("extent operation counter overflow"))?;
            expected_counter = expected_counter
                .checked_add(count)
                .ok_or_else(|| invalid("extent change counter overflow"))?;
            ops.push(Op {
                counter: op.counter,
                container,
                content: InnerContent::List(content),
            });
        }
        let change_len = u32::try_from(expected_counter - descriptor.id.counter)
            .map_err(|_| invalid("extent change length overflow"))?;
        descriptor
            .lamport
            .checked_add(change_len)
            .ok_or_else(|| invalid("extent change Lamport overflow"))?;
        changes.push(Change {
            id: descriptor.id,
            lamport: descriptor.lamport,
            timestamp: descriptor.timestamp,
            deps: descriptor.deps,
            commit_msg: descriptor.message.map(Into::into),
            ops,
        });
    }
    validate_lamports(&changes, oplog)?;
    Ok(changes)
}

// Check metadata before native import recomputes causal clocks. Unknown external
// dependencies retain ordinary pending-import behavior; their clocks cannot be
// checked until they arrive, at which point the standard importer recomputes it.
fn validate_lamports(changes: &[Change], oplog: &crate::oplog::OpLog) -> LoroResult<()> {
    let mut ranges: BTreeMap<u64, BTreeMap<i32, (i32, u32)>> = BTreeMap::new();
    for change in changes {
        ranges.entry(change.id.peer).or_default().insert(
            change.id.counter,
            (change.id.counter + change.atom_len() as i32, change.lamport),
        );
    }
    for change in changes {
        let mut expected = 0u32;
        let mut all_known = true;
        for dep in change.deps.iter() {
            let clock = oplog.dag.get_lamport(&dep).or_else(|| {
                let (&start, &(end, clock)) =
                    ranges.get(&dep.peer)?.range(..=dep.counter).next_back()?;
                (dep.counter < end).then(|| clock + (dep.counter - start) as u32)
            });
            match clock {
                Some(clock) => {
                    expected = expected.max(
                        clock
                            .checked_add(1)
                            .ok_or_else(|| invalid("extent dependency Lamport overflow"))?,
                    )
                }
                None => all_known = false,
            }
        }
        if all_known && change.lamport != expected {
            return Err(invalid(
                "extent descriptor Lamport does not match causal dependencies",
            ));
        }
    }
    Ok(())
}
