# Whole-page reclamation experiment

Verified against code 2026-11-10. Provider: `experiments/immutable-page-lease`.
This is a `#[cfg(test)]` closed-world probe, **not a supported Store GC API**.
No production payload or metadata retention behavior changed. The first green
checkpoint is `e919f31d`; the original red checkpoint is `d9265ed7`.

## What is safe in the measured fixture

`src/reclamation_probe.rs::sweep_unpublished` takes an exclusive `&mut Store`,
validates the complete extent index, and unions an explicit retained-ID set with
all live `SealedSegment` leases. It groups indexed extents by physical page.
Only a page with no retained extent and no live lease may be retired. Removing
all its index entries and decrementing the page once happen in one transaction.
The index values are ordinary `u64`s, not automatic Sanakirja page references.
The fixture contains only wrapper-created, complete, noncorrupt private pages.

The exclusive borrow prevents a new bind or insertion during the scan/commit.
Existing `BytesSlice` clones keep a segment alive; a last concurrent drop can
conservatively defer collection but cannot resurrect a zero-strong-count segment.
No weak entry is upgraded while holding its mutex: segment Drop takes that mutex.
The collector moves or copies no live payload bytes.

The probe refuses any canonical-public, derived-object or derived-head database
(root slots 1, 3 and 4). Unknown retained IDs are likewise refused before opening a mutable
transaction. The tested unknown-ID and opaque-root refusals preserve every file
byte. Slot-header/range checks are defensive, not a corrupt-store admission
protocol; the unsafe ownership contract excludes corruption. An aborted mutable
transaction has no equivalent byte-for-byte promise:
Sanakirja copies root bytes at transaction start.

`reuse_pages` proves actual engine allocation of retired pages and overwrites
only pages the allocator returns, then aborts its new reservations. It never
writes an address merely because the probe classified it as free. Successful
sweep/reopen is tested; crash/power-loss safety is not established.

## Measurements and their limits

70 extents of 256 bytes occupy five 4096-byte payload pages (14 slots per page).
The same five retained extents are concentrated on one page or scattered over
five. See `results/reclamation-lifecycle-1.97.1.log` and the JSONL beside it.

- Concentrated: four payload pages and 56 indexed extent IDs retire; all four
  pages are reused. The retained page also contains nine dead extents, which
  have payload space that cannot be reclaimed under this whole-page policy
  without relocating the live extents.
- Scattered: zero pages retire. Five live extents pin 65 otherwise-unrooted
  extents through shared physical pages. Packing alone does not solve retention
  fragmentation; grouping compatible lifetimes is an architectural concern.
- Visible deletion from 70 extents to five leaves all 70 insertion operands in
  native history. Dropping the separate old branch and historical view still
  frees zero pages while the current document's full history exists.
- Dropping the current document and historical view still frees zero pages while
  an old editable branch survives. That branch remains editable. Dropping it
  allows four pages to be reused while one cross-thread cloned handle pins the
  final page; dropping the clone releases that page.
- An unpublished branch-only insertion on its own page is reclaimed when that
  branch dies, without moving the still-live current document's page.
- After reopening with zero leases, two caller-supplied retained IDs protect two
  pages; three pages can be reused. Removing one supplied root releases its page.
  This test does **not** implement durable root discovery or a persisted root
  registry; the test supplies a complete known root set.
- Each opaque root slot 1, 3 and 4 independently forces refusal after fresh
  reopen, with no live leases and no file writes.

The file stays 262144 bytes. In the concentrated measurement its allocated
blocks rise from 36864 to 45056 bytes because metadata CoW costs space. Payload
allocator reuse is not hole punching, file truncation or returned filesystem
space. There is no end-to-end performance or storage-saving ranking here.

## Concrete identity-reuse counterexample

`characterization_retired_identity_reuse_can_reauthenticate_stale_descriptor`
repeatedly inserts ID 7 with the same length and different ASCII bytes, drops all
leases, and sweeps it. On the pinned allocator a full Descriptor repeats at
zero-based generation 2 (the third insertion). Binding the earlier Descriptor
then succeeds and returns different bytes.

This is an intentional passing **characterization of an unsafe future API**,
not a passing production-GC requirement. A bare Descriptor is not a live lease or
registered root. Today's no-reclamation public API never creates this lifecycle;
the test-only sweep exposes what would happen if promoted without identity rules.

Causes, visible directly in source:

- `sweep_unpublished` removes IDs from the only extent index.
- `packed.rs::insert_batch_bound` rejects only IDs still in that index.
- `Descriptor` and `Store::bind` compare store identity, ID, page, offset and
  length, with no generation or content commitment. All can repeat on reuse.
- `derived_next_object_id` derives max+1 from surviving objects. A future
  collector deleting the maximum object would likewise break its current
  no-reuse assumption unless a durable high-water mark survives.

Recommendation: keep logical IDs permanently non-reusable using a durable
retired-ID ledger, or a persisted high-water mark if the provider owns a
monotonic ID namespace. A generation-qualified descriptor is an alternative but
changes the descriptor/authentication contract. Never recycle an ID merely
because its payload was freed, and never derive a next ID solely from survivors.
No identity-policy change is silently added to this prototype.

## Durable reachability is the next implementation gate

`Store::publish(extents)` checks each supplied descriptor, then persists only
opaque caller metadata. It does not persist that descriptor list independently.
`derived_publish_head(required)` similarly checks object existence, then stores
only caller bytes. The arguments are validation inputs, not enumerable durable
edges. Their presence does not give a collector a graph to traverse.

`context/trusted-history-api-experiment.md` adds another important root:
`HistoryValue::Retained` still refers to the ORIGINAL immutable source. Lazy,
not-yet-read objects need protection even when no decoded handle exists. Current
visible text, current loaded handles and the latest head are each insufficient.

Before enabling published reclamation, implement and test:

1. A versioned root/dependency manifest covering accepted heads, retained
   historical cuts, durable editable branches and their original backing sources.
   Persist root publication and its full dependency edges atomically. Define
   root retirement explicitly rather than infer it from latest visibility.
2. A durable identity policy that passes stale-ID/descriptor tests across both
   reuse and reopen. Include abandoned allocations and failed publications.
3. A coherent mark/retire boundary that excludes publication/binding races;
   combine durable reachability with active leases and whole-page occupancy.
   Missing objects or unknown schema must fail closed, never mean unreachable.
4. Fresh-process tests with two retained roots sharing a dependency and a lazy
   unread history object. Retiring one root must preserve their shared objects;
   a long-lived handle must delay reuse after the final durable root is removed.
5. Failure injection at root-retirement commit, index/free-list commit and the
   next reuse. Test normal abort separately from killed-process recovery and
   publish only a fully committed result. Add multi-process scope only with an
   explicit revised ownership protocol.

For now, keep the public append-only policy and the opaque-root refusal. A
compactor that relocates live bytes would require new immutable extents and an
atomic root switch; existing raw-pointer leases and retained old roots must keep
the old pages pinned until they expire. Updating a mapping entry cannot retarget
an already-escaped pointer.

## Pinned engine boundaries

Fresh audit of cached Sanakirja 1.4.3 `environment/muttxn.rs` confirms that
`decr_rc` queues committed pages in `free_pages` (lines 656–660); allocation uses
same-transaction owned frees or the previously committed free database
(483–512). Newly retired payloads are not reused during the sweep itself.
`abort_before_reclamation_commit_preserves_logical_index_and_payloads` verifies
that aborting before commit leaves all 70 descriptors readable, and a later
successful sweep retires/reuses all five pages exactly once. It does not assert
physical file-byte rollback or simulate a failed commit.

Commit flushes data, writes roots, advances the global root and then performs
another fallible flush (`muttxn.rs:334–379`). A commit error may therefore be
ambiguous. A future collector must establish the recovery state before retrying
or reusing pages, not assume an error means nothing happened.

A second reproduced counterexample concerns root retirement itself:
`characterization_pinned_engine_remove_root_keeps_committed_root` creates a root,
calls `remove_root`, commits and reopens; the root is still present. In this
pinned source, `remove_root` records zero (410–414), but commit skips zero pending
values (361–364). Use an explicit root registry/tombstone or verify a corrected
engine behavior before implementing retirement. The current sweep does not call
`remove_root`; it preserves a valid nonzero extent-index B-tree.

Engine read transactions lock their root (`environment/mod.rs:485–493`), but
returned external leases outlive those transactions. The wrapper's own lease
census cannot be replaced with the engine's reader locks.

## Reproduce

From the repository, with Rust 1.97.1 and cached dependencies:

```sh
CARGO_NET_OFFLINE=true bash experiments/immutable-page-lease/reproduce-reclamation.sh
```

The script records toolchain/source hashes and runs all 29 provider tests,
including eleven reclamation tests. Existing dependency warnings are separate
from test results. Clippy and rustfmt were unavailable in the resumed 1.97.1
installation; neither is claimed as passed. No workspace-wide matrix or crash
fault campaign was run.

## Follow-on durable-root model

The next test-only controller implements versioned dependency manifests,
permanently increasing identity allocation and lazy-root pins. It prevents the
above ABA witness under its exclusive controller contract and tests actual
fresh-process shared-root retirement. See [rooted-reclamation-model.md](rooted-reclamation-model.md).
The original unprotected ABA characterization remains intentionally reproducible;
production Store APIs and opaque-schema retention are unchanged. The 29-test
counts and source hashes above describe checkpoint `9707c5b8`; newer full runs
also include the follow-on tests and have separately recorded evidence.
