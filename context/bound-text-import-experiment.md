# Bound JSON text import experiment

Verified against code 2026-11-07.

The feature-gated `LoroDoc::import_json_updates_with_text_extents(schema, ledger)`
accepts the existing JSON schema and requires original extent-backed insertions
from the application-owned `NativeWireLedger`. This is the provider binding half
of mixed-operation acceptance, not authentication of the complete input or proof
of application persistence. The caller must authenticate the complete schema and
receipt before this call. No wire is passed, so the ledger's wire verification
methods are intentionally not invoked.

`encoding/json_schema.rs::decode_changes_with_text_allocator` retains the normal
JSON grammar, structural/native-value/style decoding, container registration,
peer-table expansion and ID validation. The ordinary public import still uses
`alloc_str_with_slice`. The experimental allocator in `bound_text_import.rs`
resolves each text insertion by original operation ID, expanded container ID,
Unicode scalar length and byte length. All bindings must be present, complete,
nonempty, ordered, and nonoverlapping. Native-owned parts and cross-resolver
concatenation are rejected. Supplied bytes must equal the semantic schema text.
Only then does `SharedArena::alloc_verified_text_slice` adopt their descriptor
sequence and index the already authenticated schema text, with no resolver
callback under the string-arena lock.
The resulting live state and actual native Op retain the original extents.

The existing import barrier, known-history check (`ImportedValues::Lossy`, like
ordinary JSON), pending dependency application and state rollback are reused.
No second operation grammar or JSON serialization round-trip is introduced.
Lamports, timestamps, dependencies and commit messages have normal JSON import
semantics; in particular this is not a new exact native-value admission API.
Authenticated text does not make JSON's other potentially lossy values exact.

Decode-time ledger/resolver errors and panics become `Err`, allowing the existing
arena rollback to discard any earlier decoded values and text. Invalid later
schema entries also roll back. Resolver handles remain subject to the existing
immutable storage contract after admission: callbacks cannot reenter the same
Doc, return different bytes for the same extent, lose admitted extents, or retain
bounded storage views beyond a callback. A resolver that starts failing during
later native application/reads violates that contract and retains the existing
invariant-failure policy. No importer can authenticate future storage behavior.
Failed imports retain registered arena container indices, as normal rollback
requires; logical document state and published history remain unchanged.

## Copies and scope

The original `import_json_updates_with_text_extents` accepts owned schema Strings.
Its caller costs remain; physical and composite operands now compare in place
with `TextSlice::try_for_each_chunk`, without a temporary contiguous join.

`import_json_updates_with_text_references(schema, references, ledger)` instead
requires uncompressed peers, empty insertion strings, and one unique
`NativeTextReference` per insertion. Descriptors bind operation/container IDs,
provenance, ordered nonoverlapping physical ranges, byte length and scalar count.
Missing, duplicate, unused or non-text descriptors are errors. The same JSON
decoder validates counters using the descriptor lengths; all non-text grammar
and ordinary rollback remain shared. The caller must authenticate descriptors
and complete typed/receipt correspondence before admission: a self-consistent
reference alone is not an authenticity proof.

`TextSlice::try_for_each_char` validates scalars spanning physical chunks with a
four-byte stack buffer. Arena adoption prepares sparse scalar/UTF-16/byte index
metadata outside the arena lock, then installs it alongside the original slice.
The temporary index is O(bytes / 128) count metadata, not an owned text body.
Later native state operations and exports can still make temporary contiguous
joins; those remain charged to `text_slice.extent_sequence_join`. Descriptor
concatenation also retains its existing metadata-copy costs.

Native wire recipe capture now compares borrowed chunks directly with the
independently supplied wire. Reconstruction writes chunks directly into the
one final exact Update allocation, charging those writes to
`wire_recipe.reconstruct`, including partial writes on failure. It does not
first construct a contiguous text payload. These counters do not represent the
literal syntax bytes already stored by the recipe or arbitrary exporter costs.

The read-only `export_json_updates_with_text_slices` and
`export_json_in_id_span_with_text_slices` APIs share the normal JSON exporter for
all non-text operands and metadata. Insertions expose original `TextSlice`
handles separately as `NativeTextSlice`, leaving empty JSON text slots. Native
operation Unicode slicing and arena Unicode seeks use scalar cursors instead
of contiguous joins. Arena slices crossing same-resolver extent segments retain
a descriptor sequence; incompatible mixed/native segments keep the counted
fallback because the current representation cannot concatenate those owners.

Independent text comparison uses `TextSlice::try_bytes_equal`: sequential
resolver visits and a fixed 1024-byte stack buffer avoid nested cross-store locks
and payload-sized heap allocation. Actual left-hand scratch writes are charged
to `text_slice.bounded_compare`; this is bounded copying, not a claim of zero
bytes copied. Ordinary `PartialEq` also uses this path after its identity fast
path. Exact-byte comparison and all resolver callback checks remain intact.

`crates/loro/tests/bound_text_import.rs` uses real native allocations and mixed
map/list/child-text/style history. It checks compressed/uncompressed peer IDs,
original ordered physical backing in arena/OpLog/live state, exact wire export,
causal metadata, COW text edits and prefix checkout, owner lifetime, missing
parents, malformed callbacks and bindings, and invalid later schema rollback.
A rejecting original resolver additionally proves bound allocation does not call
the old arena validation/indexing resolver paths while installing metadata.
This provider test does not establish the app's hot install path, LMDB ownership,
cold recovery, crash durability, complete typed-value correctness or performance.

## Eager independent native reference oracle

`native_reference_oracle::NativeReferenceOracle` is a separate read-only owner.
Its constructors decode native bytes, not a typed schema. `NativeTextLookup`
provides only immutable original text bindings and cannot invoke receipt or
semantic verification. Every native Text insertion is decoded with its wire
operation ID, container, position and value; its borrowed UTF-8 bytes must match
the complete nonoverlapping lookup ranges before the original `TextSlice` is
adopted. Missing bindings and lookup panics are errors. Ordinary native imports
remain unchanged and charge their text-arena copies to
`native.decode.text_arena`.

`from_update` takes an already resident, reference-backed, settled base and forks
it privately. It rejects pending local edits and lazy/owned-text bases without
implicitly committing or decoding them. Incoming Update blocks are borrowed
from the input; the normal native decoder, parent registration, exact known-ID
comparison, causal import and rollback remain in use. Pending dependencies are
rejected before an oracle is returned. These preconditions deliberately do not
claim arbitrary mutable documents are zero-copy bases.

Snapshot construction uses the separate eager snapshot path: all native history
and state are decoded and validated before returning. No borrowed lookup is
retained for future lazy work. The ordinary wrapper interface exposes version,
frontier, change metadata, borrowed typed-history and deep-value reads, without
a document-extraction method. The explicitly trusted application callback below
is a separate extension of this boundary, not a hard read-only capability. Explicit
deep-value reads may allocate their requested text output. The existing snapshot
transport requires an owned encoded body buffer; that actual copy is charged to
`oracle.snapshot.encoded_body`, independently of decoded text ownership.

The `_with_lookup` recipe APIs use the same strict native binding for both
fresh-document native checks and text-span annotation. The fresh-document pass
may have unresolved dependencies, as before, because the external semantic
verifier validates the actual causal base. Receipt and semantic callbacks remain
outside the native importer, so the read-only oracle cannot recursively invoke
its caller. Strict recipe capture/reconstruction rejects missing bindings and
literal text fallback. The original recipe APIs remain the explicitly owned
control path, not a hidden fallback from failed strict admission.

TextChunk metadata construction, validation, index lookup, Unicode/UTF-8/UTF-16
boundaries, slice, split and delete now use scalar visitors. They retain constant
temporary space but currently scan O(chunk) scalars; this is not a performance
claim. Explicit string-returning operations may join the requested subrange.
Native state projections preserve the old plain `get_deep_value` equality;
style anchors and operation IDs are available as read evidence, not additional
acceptance conditions in `same_value`.

Configured snapshot oracles may inherit an empty auto-commit transaction from
`fork_cow`. Construction enters the normal import barrier on the private fork
before native snapshot decode. Real pending edits on the caller are rejected
before forking; they are never implicitly committed to prepare an oracle.

Before external resolver access, private oracle construction drains its empty
transaction, disables auto-commit renewal, and marks that private owner as
read-only for destruction. A failed private candidate is dropped normally
without attempting a commit through poisoned locks; the locks are neither
recovered nor reused, and its allocations/resolver owners are released. Normal
mutable documents keep their existing commit-on-drop behavior. The late-resolver
panic regression also checks that the failed candidate releases its resolver.


## Trusted application validation extension

`NativeReferenceOracle::trusted_validation` consumes the owner and synchronously
runs fixed application validation with `&LoroDoc` and the exact `ImportStatus`
returned by its native Update import. Snapshot constructors retain no Update
status and reject this extension; no range is guessed from the final VV. The
callback's generic application error is preserved. The oracle and its result
are returned only if the callback succeeds and before/after operation/state
versions, frontiers, pending length, detached mode, detached-editing flag, peer
identity and auto-commit state match. Errors, panics and state drift discard the
private candidate/result with the existing non-committing disposal lifecycle.

This is a trusted-code integration contract, not a sandbox or hard capability
boundary. The callback must not edit, export an encoded document, clone or retain
a Doc/handle. Borrowed history reads do not emit an encoded document.
`&LoroDoc` has interior mutability, so fences cannot prevent escaped aliases,
configuration changes, mutation followed by restoration, or later writes through
saved aliases. The executable negative control demonstrates these limitations:
a clone and timestamp-recording change pass the fence, and the saved clone can
subsequently write. Pending-length getters read real state rather than hiding
such violations behind a constant zero. Do not expose this extension to arbitrary
plugins or describe the negative control as a security guarantee.

Container ownership checks use `ContainerTrait::is_attached_to`, which compares
the exact native owner without enabling auto-commit. The existing `doc()` accessor
enables auto-commit for ordinary editing and therefore changes a private oracle's
lifecycle even if no text is edited. The validation fence continues to reject
that change; it is not reset or ignored after the callback.

Update oracles also retain the exact incoming blob metadata from their native
decode, before causal import trims already-known operations. The ordinary
metadata decoder and this path share the same reduction of sorted decoded
changes. `update_metadata()` therefore describes the wire's peer spans,
dependencies, timestamps and change count, not the candidate's final version or
the import's newly applied subset. Snapshot oracles return `None`. This lets
the application keep every original span check without invoking the ordinary
metadata API's second, text-owning decode.
