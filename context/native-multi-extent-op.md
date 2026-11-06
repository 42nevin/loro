# Native multi-extent text Ops

Verified against code 2026-11-06. This continuation is feature-gated by
`shared_arena_experiment`; see its qualification report for executed tests.

## Representation and identity

`TextSlice::ExtentSequence` owns immutable metadata through an Arc: one shared
`ExtentResolver`, ordered original extent IDs/byte ranges, and checked total byte
length. It owns no payload bytes and caches no contiguous view. Fields are private
so callers cannot install inconsistent descriptor totals. Joining two extent
slices from the same resolver copies only their descriptor metadata. The existing
single-extent contiguous fast path remains. Native-owned slices retain their
original merge rules; native/extent and cross-resolver merges are rejected.

The ordinary native `Op`/`InnerListOp` checks still require counter, container,
position and Unicode allocation adjacency. Two actual allocation calls may now
be grouped into one real native Op without requiring a physically shared extent.
A separate extent remains a separate physical part, not necessarily a separate
Op. No `PairChange` or observer-created synthetic group is used.

`ptr_eq` for sequences compares the immutable sequence Arc, never merely the
resolver, total length or logical start. Byte slicing clips the ordered parts,
keeps original physical offsets, and resolves no bytes. Unicode Op slicing uses
the existing scalar-to-byte mapping and then this byte slicing. Old sequence and
prefix handles remain immutable. Descriptor concatenation and clipping are
linear in part count; repeated merges can copy quadratic metadata overall. This
is a correctness implementation, not a demonstrated performance improvement.

## Contiguous consumers and copy accounting

Existing string consumers, Unicode indexing, serialization and native wire export
continue using TextSlice's bounded contiguous-view APIs. For a sequence,
`try_with_bytes` joins the parts into a temporary Vec, releases each resolver
view before the next part, and calls the consumer only after all parts succeeded.
`materialize` uses the same join and returns that Vec directly, without a second
copy. Neither path caches the Vec or installs it as the native operand owner.

Every part actually copied into a temporary join increments
`text_slice.extent_sequence_join` (`extent_bytes`/`extent_calls`) and calls the
resolver's `record_materialization` with the physical part ID/range. Copies before
a later missing/malformed/panicking part remain counted. The top-level consumer
never receives a partial valid operand. Resolver errors stay fallible on
`try_with_bytes`; legacy infallible consumers retain their existing panic policy.
Do not claim zero temporary copies: reads, event generation, comparison,
observation, export and validation can all join sequences. Input transfers stay
separate in `allocated.input_string`. Persistent native-owner repacking is not
introduced by sequence merge.

## Native consumers and diagnostics

All exhaustive TextSlice representation matches are in `text_slice.rs`; other
native consumers use its view/length/slicing/merge APIs. StringSlice delta merges
also use these APIs and can concatenate same-resolver extent references. No raw
pointer is exposed for either extent representation. Arena cross-segment reads
retain their existing counted copy fallback; this work does not replace that
separate path. Rich-text state, op history and arena diagnostic scans flatten
`backing_parts()` in physical order. The legacy `backing_info()` method returns a
summary with kind `extent_sequence` and no invented extent ID for a sequence.

`ObservedTextOp.backing_parts` carries every physical reference for actual merge
and slice events. Finalization compares matching native scalar slices by their
resolver and ordered physical references before accepting original insertion
ranges. Low-level Op observations still lack peer/document identity: use one
document per trace session. `native_wire_recipe::extent_refs` flattens composite
ledger operands and keeps the existing duplicate/overlap rejection. Wire grammar
and public import/export formats are unchanged.

## Application integration boundary

Separate allocations must share the exact resolver identity to merge. A wrapper
that creates a fresh resolver Arc for every extent still produces separate Ops.
An application may use its per-candidate immutable custody ledger as the stable
resolver, delegating to its stored reservation slices without holding map locks
across callbacks. It must preserve scope/store identity, bounded reads, accepted
custody and its own durability rules. Provider tests alone do not establish app
LMDB integration, durable adoption, recovery or performance gains.

The new `multi_extent_native_grouping` contract uses actual LoroDoc insertion
calls and distinct newly generated immutable extents, then checks real merge,
finalization, cross-boundary Unicode slices, partial Update import, snapshot
round-trip and resolver-owner lifetime. Additional direct representation tests
cover identity, temporary-copy accounting and failed/nested bounded views.

Join counters aggregate all requesting consumers under one dedicated join
category. The resolver's read callback still receives the requester label, but
`text_copy_metrics` alone cannot attribute joined bytes to individual requesters.
Materialization notifications occur after the part's own bounded view returns;
the TLS byte counter still records copies if a resolver subsequently panics.
A separate three-allocation native test checks exact original IDs/ranges, flattened
public state/history scans, actual fork_cow edits, historical prefix checkout,
and continued branch reads after the source document/allocator are dropped.
