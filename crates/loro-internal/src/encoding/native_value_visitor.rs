//! Experimental access to native operands in the existing JSON schema.
//! This does not define an operation format, validate admission, or import data.
use super::json::{
    FutureOp, JsonOpContent, JsonSchema, ListOp, MapOp, MovableListOp, TextOp, TreeOp,
};
use crate::encoding::value::{
    EncodedTreeMove, MarkStart, OwnedFutureValue, OwnedValue, RawTreeMove,
};
use loro_common::{ContainerID, LoroValue, ID};
use serde::{Deserialize, Serialize};

/// Structural operand location within an existing operation. List indices are
/// positions in its insertion operands, never map iteration or traversal order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "field",
    content = "index",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum JsonNativeValueField {
    ListInsert(usize),
    MovableInsert(usize),
    MovableSet,
    MapInsert,
    TextStyle,
    FutureFloat,
    FutureValue,
    FutureMark,
    FutureSet,
}

pub enum JsonNativeValueMut<'a> {
    Value(&'a mut LoroValue),
    Float(&'a mut f64),
}

// One exhaustive description of native fields in existing encoded values.
// Both the serializer visitor and exact opaque comparison use this same map.
fn native_owned_value_mut(
    value: &mut OwnedValue,
) -> Option<(JsonNativeValueField, JsonNativeValueMut<'_>)> {
    use JsonNativeValueField as F;
    match value {
        OwnedValue::F64(value) => Some((F::FutureFloat, JsonNativeValueMut::Float(value))),
        OwnedValue::LoroValue(value) => Some((F::FutureValue, JsonNativeValueMut::Value(value))),
        OwnedValue::MarkStart(MarkStart {
            len: _,
            key: _,
            value,
            info: _,
        }) => Some((F::FutureMark, JsonNativeValueMut::Value(value))),
        OwnedValue::ListSet {
            peer_idx: _,
            lamport: _,
            value,
        } => Some((F::FutureSet, JsonNativeValueMut::Value(value))),
        OwnedValue::Null
        | OwnedValue::True
        | OwnedValue::False
        | OwnedValue::I64(_)
        | OwnedValue::Str(_)
        | OwnedValue::Binary(_)
        | OwnedValue::ContainerIdx(_)
        | OwnedValue::DeleteOnce
        | OwnedValue::DeleteSeq
        | OwnedValue::DeltaInt(_)
        | OwnedValue::TreeMove(EncodedTreeMove {
            target_idx: _,
            is_parent_null: _,
            parent_idx: _,
            position: _,
        })
        | OwnedValue::RawTreeMove(RawTreeMove {
            subject_peer_idx: _,
            subject_cnt: _,
            is_parent_null: _,
            parent_peer_idx: _,
            parent_cnt: _,
            position_idx: _,
        })
        | OwnedValue::ListMove {
            from: _,
            from_idx: _,
            lamport: _,
        }
        | OwnedValue::Future(OwnedFutureValue::Unknown { kind: _, data: _ }) => None,
    }
}

enum ExactOperand {
    Float(u64),
    Value(LoroValue),
}
fn take_native_operand(value: &mut OwnedValue) -> Option<(JsonNativeValueField, ExactOperand)> {
    let (field, operand) = native_owned_value_mut(value)?;
    let operand = match operand {
        JsonNativeValueMut::Float(value) => {
            let bits = value.to_bits();
            *value = 0.0;
            ExactOperand::Float(bits)
        }
        JsonNativeValueMut::Value(value) => ExactOperand::Value(std::mem::take(value)),
    };
    Some((field, operand))
}

/// Compare opaque, already-decoded Unknown operands without interpreting them.
/// The callback compares native LoroValue types and float bits exactly; it is
/// not an admission capability. All remaining enum tags and metadata stay exact.
/// Counter and other Future subtypes are deliberately not accepted here.
pub fn unknown_operand_equal(
    a: &FutureOp,
    b: &FutureOp,
    exact_value: impl FnOnce(&LoroValue, &LoroValue) -> bool,
) -> bool {
    let (a, b) = match (a, b) {
        (FutureOp::Unknown(a), FutureOp::Unknown(b)) => (a, b),
        #[cfg(feature = "counter")]
        _ => return false,
    };
    let (mut a, mut b) = (a.clone(), b.clone());
    let av = take_native_operand(&mut a);
    let bv = take_native_operand(&mut b);
    // All LoroValue fields are now Null and all raw floats are positive zero.
    // Ordinary equality here compares only exact type/opaque metadata, never
    // NaN-equal native values or a normalized scalar representation.
    if a != b {
        return false;
    }
    match (av, bv) {
        (None, None) => true,
        (Some((af, ExactOperand::Float(a))), Some((bf, ExactOperand::Float(b)))) => {
            af == bf && a == b
        }
        (Some((af, ExactOperand::Value(a))), Some((bf, ExactOperand::Value(b)))) => {
            af == bf && exact_value(&a, &b)
        }
        _ => false,
    }
}

/// Read only the exact native Counter/F64 operand representation. Other Future
/// families and alternate scalar kinds remain distinct, even when a native
/// importer could normalize them into an equivalent counter value.
pub fn counter_operand_bits(value: &FutureOp) -> Option<u64> {
    match value {
        #[cfg(feature = "counter")]
        FutureOp::Counter(OwnedValue::F64(value)) => Some(value.to_bits()),
        _ => None,
    }
}

/// Visit all native-value operands without changing operation IDs, metadata,
/// lengths, text literals or container identity. Native lists/maps are one
/// operand; callers own their recursive representation. All current variants
/// are matched exhaustively, so adding a native-value field requires review.
pub fn visit_native_values_mut<E>(
    schema: &mut JsonSchema,
    mut visit: impl FnMut(
        usize,
        usize,
        ID,
        &ContainerID,
        JsonNativeValueField,
        JsonNativeValueMut<'_>,
    ) -> Result<(), E>,
) -> Result<(), E> {
    use JsonNativeValueField as F;
    for (change_index, change) in schema.changes.iter_mut().enumerate() {
        for (operation, op) in change.ops.iter_mut().enumerate() {
            let id = ID::new(change.id.peer, op.counter);
            let mut value =
                |field, operand| visit(change_index, operation, id, &op.container, field, operand);
            match &mut op.content {
                JsonOpContent::List(ListOp::Insert {
                    pos: _,
                    value: values,
                }) => {
                    for (index, operand) in values.iter_mut().enumerate() {
                        value(F::ListInsert(index), JsonNativeValueMut::Value(operand))?;
                    }
                }
                JsonOpContent::MovableList(MovableListOp::Insert {
                    pos: _,
                    value: values,
                }) => {
                    for (index, operand) in values.iter_mut().enumerate() {
                        value(F::MovableInsert(index), JsonNativeValueMut::Value(operand))?;
                    }
                }
                JsonOpContent::MovableList(MovableListOp::Set {
                    elem_id: _,
                    value: operand,
                }) => value(F::MovableSet, JsonNativeValueMut::Value(operand))?,
                JsonOpContent::Map(MapOp::Insert {
                    key: _,
                    value: operand,
                }) => value(F::MapInsert, JsonNativeValueMut::Value(operand))?,
                JsonOpContent::Text(TextOp::Mark {
                    start: _,
                    end: _,
                    style_key: _,
                    style_value,
                    info: _,
                }) => value(F::TextStyle, JsonNativeValueMut::Value(style_value))?,
                JsonOpContent::Future(wrapper) => {
                    let owned = match &mut wrapper.value {
                        FutureOp::Unknown(value) => value,
                        #[cfg(feature = "counter")]
                        FutureOp::Counter(value) => value,
                    };
                    if let Some((field, operand)) = native_owned_value_mut(owned) {
                        value(field, operand)?;
                    }
                }
                JsonOpContent::List(ListOp::Delete {
                    pos: _,
                    len: _,
                    start_id: _,
                })
                | JsonOpContent::MovableList(MovableListOp::Delete {
                    pos: _,
                    len: _,
                    start_id: _,
                })
                | JsonOpContent::MovableList(MovableListOp::Move {
                    from: _,
                    to: _,
                    elem_id: _,
                })
                | JsonOpContent::Map(MapOp::Delete { key: _ })
                | JsonOpContent::Text(TextOp::Insert { pos: _, text: _ })
                | JsonOpContent::Text(TextOp::Delete {
                    pos: _,
                    len: _,
                    start_id: _,
                })
                | JsonOpContent::Text(TextOp::MarkEnd)
                | JsonOpContent::Tree(TreeOp::Create {
                    target: _,
                    parent: _,
                    fractional_index: _,
                })
                | JsonOpContent::Tree(TreeOp::Move {
                    target: _,
                    parent: _,
                    fractional_index: _,
                })
                | JsonOpContent::Tree(TreeOp::Delete { target: _ }) => {}
            }
        }
    }
    Ok(())
}
