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
