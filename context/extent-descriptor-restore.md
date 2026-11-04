# Typed immutable-extent history restore

`LoroDoc::import_extent_descriptors` accepts style-free text Change descriptors,
with original change ID, dependencies, Lamport clock, timestamp/message, and
ordered insert/delete operands. Insert operands carry `TextSlice`, which may be
a native slice or an immutable logical extent ID/range plus resolver.

The API creates native `Change<Op>` records directly. It does not reconstruct
text-valued JSON, run user edits with newly assigned IDs, or import a native wire
snapshot before recovery. The usual native import pipeline checks overlap,
causal history and sequence positions, applies the DAG and rebuilds DocState.
Its existing arena and state rollback scopes cover rejected descriptors.
Recovery requires the caller's bounded database session to be available to the
resolver while indexes/state consume the old text.

Input validation rejects empty/zero-length operations, non-text containers,
negative or noncontiguous counters, invalid UTF-8, out-of-range sequence
positions, arithmetic overflow, and supplied Lamport clocks inconsistent with
available dependencies. Dependencies in the same descriptor batch are included
in this check. Missing external dependencies use ordinary native pending-import
semantics. Their supplied Lamport clock cannot be validated while absent; when
such dependencies arrive, the standard native importer recomputes the clock.
Callers performing a complete cold restore must check ImportStatus.pending and
must not treat a partial/pending restore as a complete published view.

With the experimental feature, text Change blocks in BOTH C-copy and C-ref
retain parsed operations after native serialization. Otherwise an export could
drop logical operations and a later parse silently recreate native-owned payload
copies. The matched policy retains `Changes`, not `Both`: serialization returns
wire output but does not add a retained wire cache. A per-block marker is
maintained when new changes arrive; export adds no historical scan. Outside the
experimental feature the prior byte-cache behavior remains unchanged.

`inspect_text_op_backings()` is a correctness-only metadata diagnostic. It
commits pending local edits and scans native OpLog changes, returning each text
insert's backing kind and logical extent ID/range. It does not resolve logical
payloads; serialized native-only blocks can still take the ordinary parse path.
The scan must remain outside measured edit/export paths. Tests inspect
backings after typed restore and after full native export to establish that
logical Ops were not replaced by private native heap owners.

Tests in `crates/loro/tests/extent_restore.rs` use JSON only as an outside oracle
for native fixtures. The logical resolver test supplies descriptors directly,
asserts zero materialization during restore, forces old-text reads, exports a
full native Update, edits inside the old text, and converges with a normal peer.
The ChangeStore unit test verifies logical operation owners survive repeated
native serialization instead of being replaced by decoded private payloads.

Error qualification distinguishes the fallible restore entry point from legacy
infallible consumer paths. A failed initial resolver lookup during restore is an
error and leaves history unchanged. A legacy mid-edit lookup may panic; such a
speculative branch must never be published. Its poisoned transaction lock also
causes the normal LoroDoc Drop cleanup to panic when it attempts implicit commit.
The qualification catches both the edit panic and the separate cleanup panic at
the discard boundary, then verifies that the source still reads/edits correctly.
A single catch around the edit is insufficient. That is an explicit
qualification limit, not a claim that arbitrary I/O failures have been made
recoverable throughout Loro.
