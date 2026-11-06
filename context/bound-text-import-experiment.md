# Bound JSON text import experiment

Verified against code 2026-11-06.

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

Schema creation/JSON parsing already owns the input text Strings. Those caller
costs are not removed or counted by this API. Single-extent byte comparison is in
place. A supplied composite operand, arena indexing, state work and later exports
can make temporary contiguous joins. All such TextSlice joins retain the existing
`text_slice.extent_sequence_join` copy counters and resolver materialization
notifications. The binder creates no payload Vec or persistent materialized text
owner. Descriptor concatenation still has the existing metadata-copy costs.

`crates/loro/tests/bound_text_import.rs` uses real native allocations and mixed
map/list/child-text/style history. It checks compressed/uncompressed peer IDs,
original ordered physical backing in arena/OpLog/live state, exact wire export,
causal metadata, COW text edits and prefix checkout, owner lifetime, missing
parents, malformed callbacks and bindings, and invalid later schema rollback.
A rejecting original resolver additionally proves bound allocation does not call
the old arena validation/indexing resolver paths while installing metadata.
This provider test does not establish the app's hot install path, LMDB ownership,
cold recovery, crash durability, complete typed-value correctness or performance.
