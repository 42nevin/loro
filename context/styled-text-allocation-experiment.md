# Ordinary styled text allocation experiment

Verified against code 2026-11-05. Independent tester reports three passing
contracts: twenty style-expand/Unicode-boundary combinations plus failure/bounds
and five invalid allocator cases. Tests live in
`crates/loro/tests/styled_extent_insert_contract.rs`; no broad native workspace
qualification is implied.

`LoroText::insert_unicode_with_allocator` is feature-gated and native/attached.
It shares `TextHandler::insert_with_txn_and_attr_allocated` with ordinary insertion.
The existing style-anchor placement, inherited style lookup, event position and
Unicode/entity conversion stay in that shared handler. It does not use or relax
the older style-free `insert_generated_unicode` route.

The two final text apply sites call `Transaction::apply_text_insert`. With no
allocator it performs the same normal native RawOp application. With an allocator
it captures actual peer/counter/container/Lamport/dependencies, distinct Unicode
and entity positions, and uses the existing allocation callback. Original input
bytes, returned length and UTF-8 are checked before arena adoption. The native
InsertText keeps that original returned TextSlice. Timestamp/message are not
allocation-time facts and remain later commit metadata.

Valid empty insertion performs no allocation; bounds and boundary errors happen
before allocation. Allocator callbacks/errors/panics and invalid returned content
must not consume native IDs or alter document semantics. External allocator-side
staging is caller-owned cleanup, not an accepted publication. The resolver must
permit nested immutable reads. A caller must not mutate/reenter native writes
from its allocation callback.

For an app RAM allocator, the command's existing string is copied into its first
recorded immutable native allocation. No database I/O is required by this API,
though the API itself cannot enforce a backend's scheduling policy. This is not
zero-copy command input and does not eliminate command/header text duplication.

The app hook belongs after all material-membership validation and at genesis
text creation. Accepted NativeScope history needs an immutable op-range ledger;
draft and clean publication-candidate deltas have separate lifetimes. Transfer only
accepted-candidate custody after durable publication. Persisted historical ranges
must survive deletion when old receipts/cuts can still reference them. Physical
identity absent from a legacy ledger cannot be recreated by matching equal bytes.

Generic JSON native import still allocates owned string slices. B's recording and
persistence of original native allocations is distinct from later C-ref recovery
that installs logical references in live mixed-operation Ops/DocState. The frozen
wire-skeleton proof worktree and artifact remain unchanged.

The exact-once generator assertion and catch_unwind allocation boundary predate
this hook (frozen provider base 78124e29). The new checked helper preserves that
boundary and adds returned-content validation. The adversarial double-generator
fixture prints its caught assertion as expected; no new panic-catching repair was
added to make native semantic failures appear successful.
