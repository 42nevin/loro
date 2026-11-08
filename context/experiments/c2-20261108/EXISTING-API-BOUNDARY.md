# C2: independent read-only Loro boundary review and discriminating fixture

Reviewed recovered provider pin `a64b750745dc26d279f98ca6b52f2c15351b4799` (no source edits, builds or executions in this review). References below are paths under `crates/loro/` or `crates/loro-internal/` in that pin. Test assertions are source evidence, NOT newly executed results.

## What the APIs actually promise

- `loro-internal/src/encoding.rs:26–73,108–130`: full `Snapshot` includes full history + state. `ShallowSnapshot` includes complete current state plus history after a frontier. `StateOnly` is a shallow snapshot at the target with minimal history (documented depth 1), NOT zero CRDT history and NOT just plain text. `SnapshotAt` retains all history up to the target and therefore does not implement C2.
- `loro-internal/src/loro.rs:3115–3157`: dispatch and `shallow_since_vv/frontiers/is_shallow` establish observable history boundary. `oplog_vv` equality alone cannot prove historical operations were restored: shallow documents preserve logical version knowledge while omitting operations before their boundary.
- `loro-internal/src/encoding/shallow_snapshot.rs:817,928–953`; `loro/tests/shallow_root_critical.rs:1–12,67–92`: chosen root must be a critical version, causally comparable with every retained op. Under concurrent frontiers the actual root can retreat. Record actual `shallow_since_frontiers/vv`, never assume requested latest frontier means no retained tail.
- `loro/tests/native_cow_fork.rs:56–80`: existing provider test asserts CoW fork supports a shallow document, ordinary new edit and update merge. It does not prove old-history dependency resolution.

## Compatibility cliffs worth deciding before integration

1. **An old offline edit can require deliberately discarded history.** `loro/tests/shallow_snapshot_concurrency.rs:166+` expects `ImportUpdatesThatDependsOnOutdatedVersion` for an update authored before the shallow root. `loro/tests/loro_rust_test.rs:1180–1230` also covers old pending dependencies. This is not equivalent to a fully editable full-history CRDT for arbitrary offline inputs.
2. **Importing a full snapshot back into the same shallow document is not a history hydration API.** `shallow_snapshot_concurrency.rs:140–163` explicitly asserts reimporting older/full snapshots is a no-op and leaves the shallow root unchanged. A safe first fallback experiment should construct a NEW full document from the retained archive, apply current accepted local tail + incoming update, then compare/rebind at a new publication boundary. Do not advertise in-place backfill without a new provider mechanism.
3. **Import error does not universally imply no state change.** `integration_test/shallow_snapshot_test.rs:288–333` deliberately bundles an outdated concurrent update with a valid later update: import errors, but valid `hello=world` applies and event fires. Source `loro-internal/src/loro.rs:829–875` has distinct preflight cases, and `encoding.rs:233–242` reports outdated dependencies after applying decoded changes. Use a CoW candidate and discard on failure, or examine exact success ranges; do not mutate the accepted root then blindly retry the whole batch on full history.
4. **Historical checkout is bounded.** `loro-internal/src/loro.rs:1569–1570,2683–2684` returns `SwitchToVersionBeforeShallowRoot`. A current-value match cannot establish history-query compatibility.
5. **Undo evidence is narrower than “old undo preserved”.** `loro/tests/shallow_checkout_equal_value.rs:226–248` creates a NEW UndoManager after import, edits locally, then undo/redo matches full history. This covers new edits after opening, not an old process's undo stack or undo crossing the shallow boundary. Those require separate persistence/compensation design.
6. **Deleted container metadata has semantic reachability rules.** `loro/tests/shallow_snapshot_deleted_containers.rs:1–8,45–77` drops containers deleted before the cut, but comments/tests retain tree-node metadata when moves can revive the node. Do not blanket-remove every unreachable-looking child from a state root.
7. Current app experiment `cow_scope_capsule_tests.rs::scope_from` explicitly rejects `doc.is_shallow()`. Even a provider-level green shallow experiment is not permission to simply remove this guard; source checks, native attestation and restart assumptions must be revisited explicitly.

## Minimum fixture that separates compression from genuine on-demand history

Create an author process, a ready/edit process and an independent full-history oracle process. Persist separately:
- compact editable native root (full/shallow/state-only arm), actual boundary and native IDs;
- older target-scope history archive (separate files), plus any target post-root tail;
- canonical acceptance metadata/identity/receipts, which remain required even when native history is deferred;
- pre-root offline updates and immutable oracle inputs, inaccessible to initial ready process except when explicitly demanded.

During INITIAL ready+ordinary edit, physically move all old-history archive and oracle files out of their normal paths. Instrument every archive read through a loader. Restore archive only when the tested operation signals it needs historical state. Merely comparing snapshot byte sizes or compressed archive bytes is insufficient: the full Snapshot arm embeds history internally even when external archive reads are zero.

Fixed visible content, variable H = 0/8/64 initially; alternate peers and committed insert/delete pairs so history cannot trivially coalesce into one Change. Include unrelated scopes separately from same-scope historical growth. Report root bytes, actual shallow boundary, retained native op count, old archive files/bytes opened before ready and after demand, and fallback time. OS cache state must be labelled; no physical cold-disk claim.

### Three independent adversarial branches from the same base

A. **Deleted text identity:** at V0 text `a玉🙂b`, fork offline peer. Main deletes `玉🙂`, accumulates history, then captures root R. Offline branch inserts relative to or deletes across those original characters and emits update based at V0. Add a valid post-R branch update into the same import batch to test partial-error behavior. Oracle receives all original operations. Record error, state/VV/pending/events before and after on the disposable candidate. Full fallback rebuild must preserve the already accepted new local edit exactly once.

B. **Old container identity and revival:** at V0 create nested map/text and tree node with metadata; fork offline peer. Main deletes the container / ancestor before R. Offline branch edits old child and moves a tree node to revive it. Test nested container deletion separately from tree revival (different semantics). Compare root identities and metadata, not only visible JSON; inspect `has_container` and full noncompressed op identities on the independent full oracle where available.

C. **History/undo:** retain frontiers before R, at R and after R. Checkout at R and after should match oracle where supported; pre-R checkout must explicitly fail or trigger archive recovery, not silently show current state. NEW post-open edit undo/redo must match oracle. Test restoring a pre-root undo request separately; an empty fresh UndoManager is not evidence of historical undo capability.

### Fallback acceptance criterion

On old dependency demand, open target history archive in a NEW full owner, replay accepted local tail and old update, compare against independent canonical oracle (versions/frontiers/container identity, content, source identity, receipt). Publish an equivalent replacement root only after correctness checks. Preserve old accepted root until durable publication, isolate candidate errors, and verify repeated retry does not duplicate edits. This is a conservative target-scope full recovery fallback, not proof of fine-grained historical page loading.

## Architecture interpretation

A successful shallow/state-only arm would prove “current editing can start without target's old native history, with an explicit archive fallback for older collaboration/history operations.” It would NOT prove arbitrary old updates merge without history, historical undo survives automatically, or history can be hydrated into the same Loro object. The next decision is whether that bounded compatibility contract meets the product goal; otherwise a genuine lazy-history provider needs additional implementation beyond these export APIs.

## Executed existing-API control

Added and ran `crates/loro/tests/c2_existing_api_boundary.rs`, one test with four arms (shallow/state-only × deleted-character anchor/deleted nested text). All passed against the current recovered provider plus this test, using narrow `cargo test -p loro --test c2_existing_api_boundary --features test_utils --locked` (unoptimized+debuginfo). Full fixture 668 bytes, shallow 343, state-only 342; this small size comparison is NOT a performance claim.

Both modes actually returned `ImportUpdatesThatDependsOnOutdatedVersion` for each old dependency. The failed candidate was discarded and accepted base value/VV stayed unchanged. Full snapshot reimport into the accepted shallow object preserved boundary `Frontiers([1@17])` and still rejected the old update. A NEW full owner from archive + accepted local tail + old update equalled an independent V0 oracle merged in different order (value, VV, frontiers, full noncompressed op identities); duplicate imports preserved state. New post-open undo/redo passed; checkout before root failed explicitly. This is the alternative full-recovery fallback control, not lazy-history implementation, new-process disk-read proof, or formal app acceptance.

Evidence: `c2-existing-api-build-test.log`, `c2-existing-api.json`, `c2-existing-api-source-binary.sha256` in the shared evidence root. No failing build/test attempts occurred for this file.
