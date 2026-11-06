# Native text observation experiment

Verified against code 2026-11-06. The observation and multi-extent contracts
execute under Rust 1.97.1 in the isolated continuation qualification lane.

`native_text_observation` is additive and feature-gated by
`shared_arena_experiment`. Start and drain a thread-local diagnostic trace around
allocated insertion and native commit/export. No observer callback can reenter
native mutation. Disabled observation stores no original insertion handles.
Enabled observation is outside performance measurements: it clones metadata and
retains original Op handles until drained, and finalization may resolve original
text to translate scalar intersections into byte ranges.

Insertion evidence uses the actual successful local operation, handler-supplied
Unicode position, native entity position, ID, Lamport and dependency frontiers.
Only allocated insertion routes supply this original-allocation evidence.
`Op::merge` and `Op::slice` emit their real operands and results, without changing
merge eligibility. Those low-level events have container index/counter but no
peer identity, because standalone Op has none; correlate with contextual insertion
and finalization records rather than inventing a peer. Finalized local Change
records carry actual peer/container IDs, dependencies, post-modifier timestamp
and message, native Op count, and ordered original extent ranges for each text Op.
They are emitted only after local Change admission succeeds. They describe that
transaction's Change, not an assertion about later ChangeStore coalescing.

Original ranges are intersected by native counter intervals and native scalar
slicing, and require the same backing owner/resolver as the finalized operand.
An empty original-range list means original allocation was not witnessed. This
is diagnostic evidence, not a durable ledger, rollback journal, publication
receipt or authorization to infer extent identity from equal bytes. Tentative
insertion/merge records can precede a rolled-back batch; only a finalized record
can establish the resulting local grouping. A trace is confined to its thread.

`native_text_observation_contract.rs` exercises a provider-only immutable,
preseeded same-extent control: two compatible insertions (`a`, `玉`) have adjacent
ranges and the real native RLE implementation groups them. A third insertion
(`🙂`) has another extent. With the multi-extent continuation it remains a
separate physical part inside the grouped Op. Partial native Update export
is expected to witness real scalar/byte slicing. This fixture does not establish
that the application's allocator supplies a shared extent or adopts grouped Ops.

## Caller-input copy accounting

The `text_copy_metrics()` category `allocated.input_string` counts the actual
`buffer.copy_from_slice(text.as_bytes())` in ordinary allocated insertion's
provider generator closure. Its `native_calls` and `native_bytes` fields count
caller-input transfers, not creation of long-lived native payload owners.
`extent_calls` and `extent_bytes` remain zero for this category. Count it
separately from extent materialization and retained-owner copies; do not infer
this work from the allocator's returned length. If the generator copied input
but later allocation validation failed, the actual copy still counts. The
contract asserts three closure executions and eight UTF-8 bytes for `a玉🙂`;
this assertion passes in the Rust 1.97.1 continuation qualification lane.

The feature-gated multi-extent continuation adds `ObservedTextOp.backing_parts`
for exact ordered physical references. The singular `backing` field is only a
compatibility summary for composite slices. See [native-multi-extent-op.md](native-multi-extent-op.md).
