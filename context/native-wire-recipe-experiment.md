# Native wire skeleton experiment

Verified against code 2026-11-05. Independent provider tests pass; application proof and broader qualification are separate.

`crates/loro-internal/src/native_wire_recipe.rs` is a feature-gated vertical proof
for exact native receipt bytes with original text extent references. It is not a
full lossless typed codec and does not replace any existing persistence path.

Current support is FastUpdates Text/Str payload holes only. Unbound text and all
other native syntax remain literal compatibility evidence, with retained bytes
counted. This is an intentional hybrid, not the original full-AST/no-opaque-wire
authority design. Invalid supplied bindings error rather than downgrade. Actual
Gestio admits Updates only; generic literal fallback does not expand admission or
prove context-dependent snapshot compatibility. Provider tests establish original
local allocation identity; the app proof uses receiver-local extent provenance.
Neither establishes general mixed-operation live C-ref installation.

`oplog/change_store/block_encode.rs::decode_block_inner` optionally annotates
complete `Value::Str` payload ranges for Text containers before native op RLE
merging. The byte range comes from the decoder's borrowed UTF-8 view, not a
substring search. FastUpdates stores these strings uncompressed, with a byte
length prefix outside the hole. Other wire syntax remains verbatim literal data.

`capture_native_wire_recipe` validates native input and requires an external
`NativeWireLedger` to check the application's exact receipt and independent typed
semantics. The ledger resolves a native op/container plus scalar and byte lengths to original logical extent
parts. A missing entry leaves the payload literal and counts it as fallback; an
invalid claimed entry errors. Native-owned buffers cannot masquerade as extents.

`reconstruct_native_wire_recipe` verifies recipe version, byte accounting, ordered
physical hole locations, ledger extent IDs/ranges/provenance, native validity,
external receipt and typed semantics, and freshly decoded grammar spans. Physical
occurrences may repeat the same op ID in repeated blocks; identity is not global
op-ID uniqueness. Same-content extents with different IDs are not interchangeable.

Literal fragments remain opaque receipt authority. Independent typed descriptors
remain semantic authority. This separation is deliberate and must not be called a
fully typed wire representation. Neither callbacks that return true nor importing
reconstructed native bytes as the alleged typed-only state path demonstrate the
required correspondence. General mixed-operation C-ref installation is separate.

Stats distinguish per-occurrence referenced bytes from unique referenced extent
range bytes. Copy/decode counters describe explicit recipe-layer work and exclude
native decoder allocations, callbacks, database overhead, and outer application
JSON metadata. No speed, total memory, or production adoption claim follows.

Qualification source: `crates/loro/tests/native_wire_recipe_contract.rs` is owned
by an independent tester; native accepted-syntax witnesses live in
`encoding/fast_snapshot.rs` under `lossless_recipe_contract_*`. The independent tester reports five ordinary contracts, two admitted-format
recipe cases, and three syntax witnesses through a public-library wrapper passing.
The owning internal workspace test command was blocked before compilation by an
unavailable offline fuzz dependency. Application runtime
verification and full integration remain separate gates.

Resolver contract: generated insertion nests immutable extent reads while applying
its native operation. The LMDB adapter uses reentrant locking and shared state
borrows. A test resolver holding an exclusive standard Mutex across its callback
hangs on this valid nesting; the independent fixture uses shared RwLock reads with
no concurrent writes instead. This does not prove arbitrary resolver/session
schedules safe. The first hang was located in transaction insertion followed by
rich-text chunk validation, not TextSlice equality. No provider copy workaround
was introduced for that fixture mismatch.

Application admission scope is Updates: tract-store checks EncodedBlobMode::Updates.
Generic non-Update literal fallback is not qualified for context-dependent
snapshot admission, because fresh-document validation can differ from importing
snapshot history into an existing document. Do not claim universal native-format
compatibility from the application proof.

## Native-value visitor for persisted typed schemas

The experimental `json::visit_native_values_mut` API visits the existing
`JsonSchema`'s native operands, binding each to its change/operation index,
original operation ID, container and explicit field/index. It covers List and
MovableList insertion operands, MovableList set, Map insert, Text mark, and the
existing Future raw-float, native-value, mark and list-set variants. Matches are
exhaustive over current operation and encoded-value variants. Raw unknown Future
payloads remain opaque bytes in the existing schema. The visitor itself neither
admits operations nor imports, normalizes or authenticates history.

The application uses this feature-gated visitor to replace native values with
checked placeholders while serializing a separate v2 lossless value sideband.
This does not change Loro's existing JSON/native wire formats or feature-off API.
It avoids exposing private encoded-operation value types as a second grammar.
The application owns descriptor completeness, exact scalar tags/bits, persisted
bounds and correspondence against independent native receipt evidence. Covering
a schema variant here is not proof that the application's domain admits it.

The experimental `json::counter_operand_bits` accessor exposes only the exact
Counter/F64 native representation's bits. It returns no value for Unknown or
alternate operand types, even when the generic JSON importer could normalize
them to a counter. It is a read-only representation query, not admission.

`json::unknown_operand_equal` compares only two decoded Unknown operands. It
reuses the same exhaustive native-field visitor, removes native fields from
private clones before ordinary metadata equality, and compares extracted raw
float bits or delegates native LoroValue equality to the caller's exact
type/bit predicate. No private operation AST is exported and unknown semantics
are not interpreted. Counter/Unknown pairs remain distinct. This comparison
utility does not grant application or native admission.
