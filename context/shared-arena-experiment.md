# Generated text allocation experiment

Verified against code 2026-11-04.

The opt-in `shared_arena_experiment` feature adds
`TextHandler::insert_generated_unicode` / `LoroText::insert_generated_unicode`
and `arena::TextExtentAllocator`. Default string insertion is unchanged.

## Byte path and lifetime

`TextHandler` validates a Unicode position in attached, style-free text, then
`Transaction::insert_generated_text` invokes `SharedArena::alloc_generated_text`.
The latter invokes the allocator and generator at the actual allocation site.
The generator receives exactly the reservation's mutable slice; no generated
`String` or `Vec` is passed into Loro. A backend can use a transaction-owned value
reservation for this callback. Each insertion is one extent and may stop at its
boundary; the caller must split on UTF-8 scalar boundaries.

The allocator must return an owned `append_only_bytes::BytesSlice`. In the original
append-only-bytes 0.1.12 implementation this owns native `Arc<RawBytes>`, not an
arbitrary external page owner. The later external-owner qualification below adds
an explicitly constrained alternative. `BytesSlice::from_bytes(reservation)` is therefore an explicit
payload copy and must be counted as such. This hook does **not** establish
storage-owned zero-copy live Ops. It lets a backend exercise direct reservation
generation and quantify the required ownership fallback without unsafe lifetime
extensions. Database transactions/guards can be dropped before the hook returns.

`StrArena::alloc_sealed` adopts that owner into its immutable segment tree with
no second payload copy. Index construction scans UTF-8 scalars. The returned
`InnerListOp::InsertText.slice` and live rich-text state retain native BytesSlice
owners. In the original ownership-copy backend, after the DB guard expires live Ops read
native RawBytes, not the DB page. The external-owner prototype instead retains
the committed immutable page through its independently owned lease.
`apply_local_op_converted` uses the same state, dependency, counter, event-hint,
and RLE pipeline as ordinary insertion. Different owners prevent physical RLE
merges; logical scalar IDs remain `(peer, counter + scalar_offset)`.

## Remaining copies and limitations

- The backend's reservation-to-BytesSlice fallback is a payload-sized copy.
- Native Update/snapshot encoding materializes wire bytes; native decoding
  reconstructs owned payloads. This API does not remove those copies.
- A requested arena slice spanning sealed segments joins just that range into
  AppendOnlyBytes; string-returning APIs can additionally materialize a String.
- Ordinary mutable-tail rollback copies the retained tail. Sealed generated
  segments are trimmed through immutable references instead.
- Empty, invalid UTF-8, and allocator errors publish no text atom. External DB
  writes are the allocator's responsibility and are not rolled back by Loro.
- This experimental insertion rejects styled/detached text and is native-only.
  It does not modify stable native Update encoding or existing receipt formats.

`crates/loro/tests/shared_arena_experiment.rs` covers generation followed by
reservation destruction, G1 `a`, reopen, G2 `玉`, historical G1 checkout, one new
peer atom on native Update sync, tiny scalar extents, mixed UTF-8, middle edits,
CoW branch abandonment, concurrent causal histories, and invalid UTF-8 recovery.
The StrArena unit test covers rollback across generated sealed segments while
another branch retains the full history.

## Qualification commands

With Rust 1.97.1 and cached dependencies:

- `cargo test --offline -p loro --features shared_arena_experiment --test shared_arena_experiment`: 5 passed.
- `cargo test --offline -p loro-internal --lib --features shared_arena_experiment generated_sealed_owner_rollback`: 1 passed.
- `cargo test --offline -p loro --test native_cow_fork`: 4 passed with default features.
- `cargo check --offline -p loro-internal --all-features`: blocked before compilation by uncached `bumpalo 3.19.0`. Native focused builds passed; all-features compilation remains unverified.

Additional checks compare six extent-backed RLE runs with one ordinary packed
run after expanding both to identical scalar `(counter, position, character)`
records. The G2 check inspects decoded counter 1, position 1, operand `玉`, and
one atom, not just version-vector advancement. Sealed-prefix pointer identity
checks establish zero old-prefix payload copies at adoption. Allocation failure
and generator panic return errors before arena mutation; the latter is caught
inside the allocation hook to avoid poisoning the auto-transaction lock.
A cleared-style-history test rejects insertion before generation if hidden
anchors remain. Threaded CoW branches exercise independent owner lifetimes.

## Allocation-time native operands

The additive `TextExtentAllocator::begin_allocation(&TextAllocationContext)`
callback receives the transaction's actual next atom ID, container ID, Unicode
and entity positions, Lamport clock, and causal dependencies before `allocate`
or generation runs. The first local operation uses the transaction's initial
frontier, which may have multiple concurrent heads. Later operations use the
immediate preceding local atom. These are the same fields used by the normal
transaction apply/version pipeline, not predictions made by a storage caller.
The hook runs inside the same panic boundary; rejection leaves the generator,
reservation allocation, arena, and local atom count untouched.

Storage implementations can validate their proposed records or construct them
from this context before committing payload+metadata. This is still not a
cross-database atomic transaction with Loro publication: a crash after storage
commit but before Loro apply requires the experiment's existing recovery rules.

Commit timestamp and message are deliberately not supplied as final values.
Commit options (and pre-commit behavior) can change them after allocation. This
qualification uses timestamp 0 and message None explicitly; other metadata
policies require finalization at commit rather than assuming allocation-time
values are final. The API makes no timestamp promise.

The allocation-context regression rejects deliberately wrong IDs and dependency
frontiers before allocation/generation, then verifies a real two-head concurrent
frontier, a nonempty same-peer history, and predecessor dependencies for the next
local run in the same transaction.

The later published-stage recheck also completed online all-features compilation
successfully after fetching the missing registry dependency; this supersedes the
initial offline cache-miss limitation above. Focused clippy completed with existing
style warnings and no errors.

## External-owner qualification

The `codex/shared-arena-owners-20261104` slice vendors append-only-bytes 0.1.12
and adds an unsafe `ImmutableBytes` trait plus safe `BytesSlice::from_immutable`.
Its contract requires initialized, immutable, address-stable storage retained
through the final owner, including protection from external mutation/reclamation.
This is not a lifetime cast of a database borrow. Existing native owner allocation
and cross-segment copy paths remain unchanged. RawBytes gains an optional external
fat Arc pointer (additional allocation metadata); BytesSlice layout is unchanged.

`experiments/immutable-page-lease` implements a deliberately narrow Sanakirja
1.4.3/core 1.4.1 page owner. It privately owns the environment, commits before
exposing bytes, never reclaims payload pages and requires unsafe exclusive-file
opening through the final derived owner. Ordinary metadata B-tree maintenance
continues. Database guards are dropped before returning the immutable owner.
No ownership-transfer payload copy occurs, verified with pointer identity.

This establishes feasibility under explicit storage invariants, not qualification
of arbitrary LMDB/Turso values or production GC. It is separate from the full
A–D benchmark and does not implement Gestum publication. Native snapshot recovery
still copies payloads. See the prototype README for the exact safety boundary,
remaining copies, limitations and reproduction command.
