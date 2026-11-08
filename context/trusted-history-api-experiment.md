# Trusted history API experiment

Verified against code 2026-11-08. Feature: `trusted_history_experiment` in `loro`
and `loro-internal`, disabled by default. This promotes the measured test seam
into an opt-in provider API for a coordinated application experiment. It is not
a remote snapshot format or a production storage service.

## Contract

`LoroDoc::open_trusted_history(state, entries, source)` restores a complete current
state and full historical VV/key directory from a trusted accepted local cut.
`TrustedHistorySource::read(key)` must return immutable bytes from that exact
root and verify the application's object commitments. An error is a storage
fault, never proof that an operation is absent. The source must pin its objects
while any branch can reference them. Shallow histories are refused. Ordinary
external snapshot import retains its existing full validation.

`LoroDoc::capture_trusted_history()` runs at a commit barrier, encodes current
state and flushes dirty ChangeBlocks. It returns the full key directory with:

- `HistoryValue::Retained { bytes_len }`: exactly that key in the ORIGINAL
  attached immutable source. Preserve the root's old object reference.
- `HistoryValue::Written(Vec<u8>)`: an overlay value requiring persistence.
  A capture is not publication or rebase. Capturing again keeps unpersisted
  overlay values Written, even after a previous capture.

The application must persist Written objects and current state and atomically
publish their references together with its complete accepted metadata. Retained
objects remain unchanged. Reopen the resulting root to establish a new base.
`bootstrap_full_export` is true for an initially unbacked owner, where capture
reads every history KV value once; it is false for the backed path. Applications
should reject true on a supposed ordinary backed publication path. The provider
does not silently materialize history and then call it incremental.

`fork_cow` preserves the immutable backing and copies private overlays. Ordinary
snapshot `fork`/export APIs can still materialize history; using them can lose
backing and will be exposed by the capture flag. The full key directory and
current-state encoding are still eager. This is not bounded metadata recovery.

## Storage failure containment

Existing KV get/scan APIs are infallible. The experimental adapter raises a
private `StorageFault` and latches a feature-only per-document failure marker.
`guard_trusted_history` catches ONLY that payload; unrelated panics propagate.
Wrap the entire isolated candidate operation (prepare/check/import/install/
capture), propagate both the guard error and the operation's own Result, and
publish only after the complete operation succeeds.

A failed candidate is **not rolled back or usable**. Immediately discard it.
Ordinary read/edit APIs after failure are unsupported and may see poisoned locks;
do not clear lock poison or continue. Capture and CoW fork explicitly refuse
before acquiring poisoned locks. Actual document/transaction/commit-guard Drop
skips auto-commit ONLY for a marked failed candidate, so disposal neither retries
I/O nor publishes unfinished work. The marker is shared among ordinary handles
of the same owner but independent in each CoW branch. Default-feature and ordinary
panic behavior are unchanged.

This detail was learned from a real failing public-wrapper test: the original
catch-only approach poisoned non-test mutexes and automatic Drop caused a double
panic/abort. That failure was retained in the evidence; a core-only happy path did
not establish safe disposal. No memory leak/`mem::forget` workaround is used.

## Focused checks

`crates/loro/tests/trusted_history_capture.rs` covers public feature use:
bootstrap, lazy reopen, actual CoW edit, capture with zero additional historical
source reads in the fixture, repeated capture and a second edit against the
original source, old-device import and backed recapture, Written/Retained root
reconstruction, actual failed-candidate Drop, independent accepted owner,
retry from a good reopened source, and unrelated-panic propagation.

Run `cargo test -p loro --test trusted_history_capture --features
trusted_history_experiment --locked`. Also run default `cargo check -p loro
--lib --locked`, and feature-enabled internal `import_atomicity`, snapshot and
CoW regressions. Record results separately; this document describes coverage,
not proof of all CRDT/storage cases. The application integration must still prove
its own root publication, retry, writer claim and full canonical oracle.
