# Actual Loro history and page reclamation

Verified against code 2026-11-10. Adapter:
`experiments/immutable-page-lease/src/loro_reclamation.rs`, nested under the
private test-only rooted controller. This follow-on uses real `LoroDoc`,
`capture_trusted_history`, `fork_cow`, and `TrustedHistorySource`; the previous
[durable-root model](rooted-reclamation-model.md) supplies storage bookkeeping.
No core Loro source, production GC API or canonical application journal changed.

## Authoritative dependency enumeration

The adapter persists the **entire actual capture**: current-state bytes plus
every history key, including `vv`, `fr` and historical ChangeBlock bodies.
It never derives retention from current visible text or loaded handles.

- `HistoryValue::Written(bytes)` creates a new committed immutable blob object.
- `HistoryValue::Retained { bytes_len }` preserves exactly that key's object
  reference from the document's ORIGINAL attached source. Length must match.
- A `BackedDoc` keeps that original source through captures and CoW forks.
  Publishing a capture does not rebase the document. Reopening the published
  root creates a new backing source.
- Backed publication refuses `bootstrap_full_export`; only initial unbacked
  bootstrap is allowed to materialize all history.

An immutable document manifest maps keys to object IDs, lengths and Blake3
commitments and identifies the state object. A trusted root envelope commits the
manifest blob. The durable root graph explicitly includes the manifest, current
state and every history object. Reopen checks that its direct children equal
that dependency set; state, manifest and source-read blobs are length/hash
verified. All objects are persisted before publishing the root snapshot.

This enumerates dependencies of these captured Loro roots. The application still
chooses which roots to retain or retire. It does not enumerate unrelated opaque
canonical records or derive application acceptance/receipt policy. The trusted
history API remains a trusted-local admission path, not remote import validation.

## Real lazy-source ownership

`DiskSource` implements Loro's actual `TrustedHistorySource`. It holds an immutable
key map, a `RootPin`, and shared access to the controller. Loro's internal CoW
backing retains this source. The source returns decoded immutable object bytes,
not a replacement snapshot or an absent-value fallback.

A source pin protects the complete root closure, including unread historical
objects. This is deliberately conservative: it also keeps superseded state and
manifest payloads while the source survives, even if those parts no longer need
to be read. Optimizing that closure is not part of this result.

The document-only ownership test drops the accepted owner and explicitly
removes/drops `BackedDoc`'s extra source Arc. Only the actual `LoroDoc` branch's
internal backing remains. It still prevents collection, supports another private
edit, and releases its old root pin when that document is dropped.

Store locks are released before all Loro calls, because historical callbacks
synchronously acquire the controller lock. Capture finishes before persistence
locks the controller. Errors leave the source callback after releasing that lock;
`guard_trusted_history` then contains the failed candidate. Failed candidates
cannot publish a new root and are discarded, never repaired by clearing poison.

## Four-process experiment

The driver launches four separate OS processes with the test executable. Each
normal process exit drops its mappings and owners; this is not crash recovery.

1. **Bootstrap real deleted history.** An ordinary Loro document creates
   `abcdef`, then performs 32 text insertion/deletion rounds with four editing
   peers. Its latest visible text is still six bytes, but full capture contains
   35 actual history KV keys. A saved frontier lies before the first deletion,
   when the inserted text was visible. A separate old device has an update based
   on the earlier six-byte state.
2. **CoW edit and old-root retirement.** Reopen from the stored current state and
   actual lazy source, fork with `fork_cow`, edit and capture. The result retains
   33 original history object references and writes three KV objects. The source
   read delta around capture plus persistence is zero. A new durable root shares
   those exact objects. Retire root0, drop the accepted owner and the adapter's
   extra source Arc: the actual old Loro branch alone still prevents collection.
   It remains editable. Once dropped, eight root0-exclusive pages are returned
   by the engine allocator and overwritten; shared history stays retained.
3. **Historical checkout and late-device sync.** In a fresh process, reconstruct
   root1 using only the persisted state/directory/source. There are 33 unread
   history keys at open. Checkout of the saved frontier fetches old source bodies
   and restores the previously deleted text. Reattach to latest and import the
   old-device update; state, full VV and frontiers match an independent ordinary
   Loro oracle. Capture the merged root with 34 retained history objects. Once
   root1 and its source retire, another eight exclusive pages are allocator-reused.
4. **Full history and final release.** Reopen root2, export through the real lazy
   source, and import that export into a separate ordinary document. Compare
   state, VV and frontiers with the independent oracle. Also compare the entire
   exported JSON change history, including operations and metadata; only change
   vector order and dependency vector order are normalized. Historical checkout
   still restores the saved deleted text. Only after explicit retirement of the
   final root and release of its owner are the remaining 75 payload pages reused.

The independent ordinary snapshot is test-oracle input only. It is never used to
initialize the document under test or as a fallback source. Lazy-read counters
are checked before full-history oracle/export checks warm the source.

`reuse_pages` allocates and overwrites only engine-returned free pages, then
aborts those new reservations. This proves allocator-issued reuse and that live
history survives the overwrite; it does not publish replacement objects or
establish their crash durability.

## What can actually be retired

Visible deletion does **not** authorize removal of the corresponding history.
The new current root remains non-shallow and retains all logical historical
obligations, either through the unchanged original objects or new complete
representations of rewritten KV values. The full-history equality check and old
frontier/old-device tests cover this fixture after real page reuse.

The eight-page results reclaim superseded state/manifest/history-KV
representations unique to explicitly retired roots. They do not discard the
operations merely because their text is no longer visible. Shared bodies remain
reachable from the latest authoritative directory. Root pins additionally prevent
retiring representations still accessible to live old documents or branches.

Deleting the final root is an explicit test policy decision to abandon this
entire private fixture. It is not a proposed automatic policy for user history.

## Failure and integrity evidence

A separate real-Loro test injects a source-read error during a fork's full export.
The guard rejects the candidate, its capture/publication refuses, and the durable
root count does not advance. After discarding it and restoring the source, the
accepted owner remains usable. A deliberately wrong Blake3 commitment is refused
by the shared blob validator. The latter is not a raw disk-corruption or damaged
blob callback test.

The earlier controller tests still cover identity non-reuse, tombstones, lazy
pins, packed pages, precommit abort and lost acknowledgment after successful
commits. No new claim is made about failed fsync, torn writes, power loss, SIGKILL,
media errors or concurrent external writers.

## Encoding and scope limits

The provider currently accepts only UTF-8 payloads, while actual Loro state and
history objects are binary. This fixture hex-encodes and page-chunks them, with
explicit allocation/encoding/decoding copies and roughly 2x encoded body bytes.
It is not a direct original-text extent binding, zero-copy recovery design or
performance benchmark. Metadata snapshots remain unbounded and permanently
retained. Payload reuse does not imply file shrinking or net disk savings.

Coverage is one substantial text-history workflow, not every Loro container,
merge shape, root policy or application transaction. Canonical application
publication/receipt integration and production schema/migration are still absent.
The next appropriate qualification is real storage-fault/crash testing and the
application's root publication/retirement contract, rather than dropping more
history based on the visible document.

## Reproduce

```sh
CARGO_NET_OFFLINE=true bash experiments/immutable-page-lease/reproduce-loro-reclamation.sh
```

The script records source hashes, runs the complete provider suite (43 top-level
test entries, including two subprocess helpers), and checks the non-test library.
The suite includes the synthetic four-process scenario and the actual-Loro
four-process scenario. Logs and 26 measurement rows are retained under
`experiments/immutable-page-lease/results/loro-reclamation*`.
The native workspace-wide suite was not run; no core source was modified.
Existing dependency warnings remain; Clippy/rustfmt are unavailable in the
resumed toolchain and are not claimed as passed.
