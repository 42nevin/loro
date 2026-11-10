# Durable-root reclamation model

Verified against code 2026-11-10. Implementation:
`experiments/immutable-page-lease/src/rooted_reclamation.rs`.
This is a private `#[cfg(test)]` storage controller, not a production Store API
or automatic Loro root enumerator. It follows the hazards measured in
[page-reclamation-experiment.md](page-reclamation-experiment.md).

## Persisted protocol

Root slot 5 indexes immutable, versioned manifest snapshots. Each snapshot has
an explicit format, schema version, sequence and Store identity, plus:

- Permanently increasing extent, object and root identity counters.
- Immutable objects containing complete extent Descriptors and child-object IDs.
- Named-by-ID roots with a Current, History or EditingBranch kind, a target
  object and an active/tombstoned flag.

The latest sequence is authoritative. Opening an existing file requires that
manifest; it never adopts legacy indexed extents or resets counters. Unsupported
or malformed latest snapshots are refused without falling back to older roots
or older identity counters. Tests cover bad schema, JSON, graph, counters and
active descriptors, with byte-for-byte unchanged files on refusal.

A root mutation publishes a complete new snapshot in one existing
`Store::publish_record` transaction. Its roots and dependency graph therefore
share a commit boundary. An object may be stored before any root points to it;
that object is not automatically a retention root. Retiring a root persists an
inactive tombstone, avoiding the pinned engine's ineffective `remove_root` path.
Old snapshots, object metadata and tombstones are retained forever in this model.

Objects form a DAG: children must already exist and have smaller object IDs.
All metadata is checked for structural/counter consistency, but only reachable
objects require present payloads. Dead object descriptions can legitimately refer
to collected payloads; reopen must not reject its own retired history. Publishing
another root to an old object revalidates every transitive dependency and refuses
if any were collected. It cannot resurrect them from metadata alone.

## Permanently issued extent identities

The controller exclusively owns its Store and does not expose its unrestricted
ID-taking insertion methods. It commits the next-extent counter **before** a
payload allocation. `ReservedExtent` is a non-Clone, one-use capability returned
only after that reservation succeeds. Allocation consumes it, including on
failure. Dropping an unused reservation burns the ID. A batch reserves its whole
range before generating any packed payloads. Counters never derive from the
maximum surviving index entry, and overflow is refused before mutation.

This is a persisted high-water policy, not a descriptor generation-field change.
Object and root IDs likewise increase in committed snapshots and are not reused.
The original public Store still permits caller-supplied IDs; this protection
exists only when the file is managed exclusively through this controller.

The previous ABA witness remains as a characterization of the unprotected probe.
The protected test reaches the same physical page/offset/length again at
zero-based generation 2, with a different logical ID. The old full descriptor
is rejected before and after reopen, while the new descriptor still reads the
new bytes. This verifies actual reuse rather than merely comparing fresh IDs.

Abandoned ID 0, invalid-UTF8 ID 1 and panicking-generator ID 2 stay burned after
reopen; the next successful allocation gets ID 3.

## Durable roots, unread readers and byte leases

`mark` traces the union of active durable roots and in-memory lazy-root pins.
It authenticates referenced IDs, locations and slot headers against the durable
extent index, without creating payload leases or decoding text bodies. These
metadata/header reads are real storage I/O; zero payload bindings is not zero I/O.

`RootPin` records an immutable target object and retains the Store Owner and its
exclusion locks. A durable root may be retired while an existing pin still keeps
its entire unread dependency closure live. Clones share the pin until the last
clone drops. If the controller is dropped first, that last pin still blocks a new
Store mapping, so reopening cannot silently lose the pin registry. Such a pin
retains data but is not an independently usable reader after its controller dies.

The controller obtains an exclusive mutable borrow across mark and sweep. New
roots, reservations, pins or bindings cannot appear in that interval. Existing
pins and byte leases may clone/drop, which can conservatively delay collection
but cannot introduce a previously unmarked target. The page collector combines
the traced extent IDs with live segment leases and frees only wholly dead pages.
No live payload is relocated.

The original unpublished sweep now refuses root slot 5. The shared test-only
primitive requires the exact validated manifest sequence, checked again before
mutation. Opaque public/object/head roots at slots 1/3/4 remain refused; this
model does not infer their dependencies or migrate their formats.

## Experiments

`shared_roots_and_lazy_history_survive_fresh_processes` launches four separate
OS processes using the test executable. No old mapping, in-memory root set or
lease is carried from one phase into the next:

1. Create two durable roots sharing one object, with one unique object each and
   one unrooted payload. Drop all owners and exit normally.
2. Reopen and collect the orphan. Retire the historical root and collect only
   its unique payload. The shared and current-only objects remain available.
   No payload binding occurs until the final explicit reads in this phase.
3. Reopen, pin the remaining unread root, then tombstone it. Zero pages collect
   while its cloned lazy pin survives. After reading the shared bytes and
   releasing the pin, only the current-only page collects. Dropping the final
   byte lease permits collection of the shared page.
4. Reopen with both roots retired. Dead metadata is admitted but cannot be
   republished; extent/object/root counters continue at IDs 4/5/2.

Other cases cover two packed pages with one live root per page, final byte-lease
pinning, a pin outliving its controller, foreign pins/reservations, counter
exhaustion, invalid latest manifests, missing legacy manifests, and refusing an
untraced or stale-sequence sweep. Both packed pages are eventually allocated and
overwritten through the engine after their respective retention ends.

## Errors and what has not been established

The controller latches fail-stop before every manifest, payload and sweep commit
path. On an error or caught generator panic, further controller operations refuse.
Drop the controller and release every surviving pin/byte lease before reopening
to discover the authoritative state; those owners keep the exclusion lock until
they expire. Do not retry with cached roots, counters or a previous sweep plan.
Existing immutable byte handles remain readable until dropped; unsafe external
file mutation remains excluded.

`lost_success_after_actual_commits_fail_stops_and_reopens_disk_state` injects an
error **after a real successful commit but before its acknowledgment** for root
retirement, payload insertion and sweep. It proves that stale local state cannot
be reused, repeated calls do not mutate the file, and reopen sees the actual
committed outcome. It does not inject a failure inside Sanakirja's commit or fsync.
The existing abort-before-commit test establishes a separate boundary.

No SIGKILL, power-loss, torn-write, media-error, disk-full or actual engine-I/O
failure qualification is claimed. Recovery after those cases remains a required
next step. Normal fresh-process reopen is not a crash test.

At this model checkpoint, the Current/History/EditingBranch labels and lazy
objects are a synthetic storage graph. The model alone does not integrate
discovery with actual Loro accepted heads,
`TrustedHistorySource`, `HistoryValue::Retained`, historical cuts or branch
publication. The earlier native-document tests cover actual Loro handle lifetimes,
but combining those paths with durable manifest publication is still work.

Full manifest snapshots and permanently retained metadata/counters grow without
bound; graph scans and authentication are not optimized. Payload reuse does not
prove net disk-space savings or file shrinking. No production API, migration,
compaction, multiwriter support or performance result is supplied.

## Reproduce and evidence

With Rust 1.97.1 and cached dependencies:

```sh
CARGO_NET_OFFLINE=true bash experiments/immutable-page-lease/reproduce-rooted-reclamation.sh
```

The script records source hashes, runs the complete provider suite (40 top-level
tests, including 11 rooted-test entries and a four-process scenario), and checks
the non-test library build. One rooted entry is the subprocess helper and is a
no-op in the ordinary top-level run; the driver executes its four real phases.
Logs and JSONL live in `experiments/immutable-page-lease/results/` under the
`rooted-reclamation` prefix. Existing dependency warnings remain. Clippy and
rustfmt are not installed in this resumed toolchain; neither is claimed to pass.

## Actual-Loro follow-on

A subsequent adapter now obtains the dependency directory from real Loro capture
and implements the actual `TrustedHistorySource` callback. It verifies old CoW
source retention, historical checkout, late-device sync and complete normalized
history after allocator reuse. See [loro-history-reclamation-integration.md](loro-history-reclamation-integration.md).
The earlier synthetic tests and their 40-test checkpoint evidence remain distinct;
the new full run includes both and records its own source hashes.
