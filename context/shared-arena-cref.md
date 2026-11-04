# Logical text extent qualification (C-copy / C-ref)

This is an experimental text representation, not a general-purpose production
storage API. Both lanes use the same fork. C-copy returns a native BytesSlice
from the actual generation hook; C-ref returns a TextSlice containing only an
Arc resolver, immutable extent ID and byte range. No private payload cache is
present in TextSlice or live rich-text chunks.

## Storage interface

`TextExtentAllocator::allocate_text` is additive: its default wraps the original
`allocate` result. A C-ref allocator overrides it and returns
`TextSlice::extent(resolver, id, range)`. The real allocation context remains the
source of native atom ID, positions, Lamport and dependencies.

`ExtentResolver::with_bytes(id, range, consumer, visit)` resolves a bounded borrowed
view. The visitor cannot return a reference tied to that view. Implementations
must call it exactly once, keep the bytes valid only through the callback and
retain the logical extent while live/durable references require it. Handles do
not pin DB transactions. The adapter decides whether the bounded session is the
current writer (precommit), a fresh reader, or a reader explicitly reused across
an edit/export. No raw pointer or borrowed transaction escapes through TextSlice.

Resolver `record_materialization` records temporary heap copies by extent,
range, consumer and byte count. Requested output strings and native encoded wire
buffers are separate output bytes, not zero-copy claims.

## Consumer map

Scoped direct reads, without a temporary payload String:

- allocation UTF-8 validation and arena Unicode-index construction;
- generated native operation application;
- Op Unicode slicing and comparison;
- StringSlice validation/count/index/split/delete/serialization;
- rich-text TextChunk validation/count/index/split/delete and old-text access;
- native operation value encoding (`wire.encode_op`);
- output text/delta assembly copies directly into the requested output buffer.

Explicit returned contiguous views use temporary Cow-owned bytes for C-ref,
never cached in the operation/state: `StringSlice::as_str`, `StrSlice::as_str`,
`TextChunk::as_str`, `TextChunk::unicode_slice`, and arena string-range access.
Substring requests copy only the requested byte range. Native-backed variants
remain borrowed. `arena.cross_extent_join` copies only the requested joined
range and records each contributing extent/range. `oplog.raw_op` materializes
when returning a separately owned raw operand. Cross-store text equality may
materialize the left operand to avoid opposite store-lock ordering. JSON export
is labeled `oracle.json_export` and is not the recovery path.

## Recovery

`import_extent_descriptors` constructs native Change/Op values directly from
typed original IDs, dependencies, Lamport, timestamp/message, container, positions,
and insertion handles/deletion operands. It adopts payload handles into the
arena and invokes native DAG/state import. It does not reconstruct JSON text or
export/import a snapshot. Unknown dependencies use native pending-import behavior.
The API currently supports style-free text insertions/deletions only.

Under `shared_arena_experiment`, text blocks in BOTH C-copy and C-ref retain parsed
Ops when encoded, and do not install an additional per-block wire-output cache.
This symmetrical policy prevents C-ref handles becoming decoded native copies,
without giving C-ref an unfair avoided-reparse advantage over C-copy. It is a
representation requirement, not a credited speedup. Default baseline builds keep
their original cache policy. A per-block flag is maintained when changes enter a
block, not discovered by scanning history at export. Returned wire output and any
other pre-existing storage caches must still be accounted separately.

## Failure boundary and limits

Fallible generation/recovery validation calls return resolver errors before
publishing operations. Existing infallible native consumer traits cannot return
storage errors: unexpected resolution failure there panics. This is explicitly a
qualification limitation. Callers must isolate speculative candidates, discard a
failed candidate, and never publish it. Reusing a poisoned candidate is not a
supported recovery strategy. Failed database commit must also discard its
speculative Loro state and leave the published root unchanged; the LMDB harness
owns that transactional test.

The resolver must not recycle extent IDs or mutate their bytes. Reclamation,
cross-process reachability, corrupt-store repair, arbitrary styles/container
kinds, and a fully fallible Loro API are not solved by this representation.
The LMDB paired harness supplies publication/durability and measured timing; unit
tests here are not a database performance ranking or power-loss qualification.

## Symmetric copy counters

`reset_text_copy_metrics()` and `text_copy_metrics()` expose experimental
per-thread counts by consumer, split into `native_bytes`, `extent_bytes`,
`native_calls` and `extent_calls`. Reset and snapshot on the measured edit thread.
Cross-extent joins count every copied contribution, including native sealed
segments and the mutable tail; absence of a resolver callback is not evidence
that C-copy copied zero bytes. TextSlice::to_vec counts native and logical actual
copies. Borrowed native views count zero. Per-extent resolver callbacks remain
available for attribution, but the universal counters are the comparable totals.
Native-owner allocation/input transfer and requested output serialization remain
separate harness stages to avoid conflating them with these temporary copies.

The matched external consumer can use pristine crates.io append-only-bytes
0.1.12 without this repository's root patch (Cargo patches are not transitive).
C-ref production code uses only its original APIs. That removes the deferred
Sanakirja external-owner extension's extra RawBytes metadata from C-copy costs.
The older Sanakirja owner tests in this workspace still use the vendored extension
and remain a separately qualified prototype, not part of the matched LMDB pair.
