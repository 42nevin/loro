# C2: current-state restore with demand-loaded full history

Baseline: `a64b750745dc26d279f98ca6b52f2c15351b4799`. Test-only provider experiment; application's production provider pin is unchanged. The ordinary untrusted snapshot import and CRDT algorithms are unchanged. `ChangeStore::import_all` still validates imported storage bytes; its existing post-import metadata initialization was extracted into a shared helper.

## Decision result

**This fixture can show current text and make a real CoW local edit without reading most historical block bodies. Old-device concurrent updates still converge correctly, but the tested old update reads the entire history.** This establishes a useful boundary, not a production-ready lazy-history system or a guarantee of bounded remote merge.

Startup is not O(1). The complete key/reference manifest remains eager and grew to about 505 KB for 2,049 history blocks, even though historical body reads stayed small. Current state is a separate complete 89-byte encoded state in this deliberately small-text fixture.

## Method and trust boundary

A fixture author first creates and validates an ordinary full snapshot, then writes current state and one file per history KV entry. Author exits. Reader, independent enumeration reader, and fault probe run as separate OS processes. `DiskKv` retains key/path/length/checksum metadata, not history payloads. Reads happen on `get` or iterator consumption; mutations use a private overlay and CoW clones share immutable references. No page-cache flush is performed: these are actual logical file reads, not cold-device I/O measurements.

This is a **trusted local capture**. Per-object xxhash32 is accidental-corruption detection, not authentication against malicious substitution. Ordinary external import does not gain permission to skip its existing validation. Full history remains available, VV is preserved and `is_shallow()` is false.

H counts insert/delete pairs with 1,160-character-scale temporary text while final text stays short. `single` uses one author peer; `multi` rotates four peers after a genesis peer. Actual data was not coalesced away in either arm: H64 yields 129 Changes/blocks and H1024 yields 2,049 Changes/blocks. Do not describe the single arm as a one-Change compressed history.

The old device forks from `abcdef玉🙂`, inserts `OLD` between original `b|c`, then deletes original `c`. The current branch later deletes original `b`. Thus the old insertion is adjacent to a character deleted on the current branch; this is not merely a fresh edit at the latest version.

## Measured history reads

Counts below are cumulative at the named stage. “Distinct” is unique historical block keys; actual reads can revisit the same block. Manifest and state bytes are separate and must not be omitted.

| Arm | Changes / blocks | Historical body bytes stored | Eager manifest bytes | Current state bytes | Attach distinct history blocks | Attach history bytes read | After old merge distinct blocks | After old merge actual history reads / bytes |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| single H0 | 1 / 1 | 87 | 736 | 89 | 1 | 174 | 1 | 10 / 870 |
| single H64 | 129 / 129 | 83,947 | 32,094 | 89 | 1 | 176 | 129 | 392 / 252,311 |
| single H1024 | 2,049 / 2,049 | 1,343,560 | 506,613 | 89 | 1 | 184 | 2,049 | 6,152 / 4,031,178 |
| multi H0 | 1 / 1 | 87 | 735 | 89 | 1 | 174 | 1 | 10 / 870 |
| multi H64 | 129 / 129 | 84,647 | 32,059 | 89 | 5 | 510 | 129 | 396 / 254,790 |
| multi H1024 | 2,049 / 2,049 | 1,354,967 | 505,135 | 89 | 5 | 515 | 2,049 | 6,156 / 4,065,760 |

For all six fixtures, known-text access and CoW first local edit added **zero historical file reads** after attach. In the separate fresh enumeration process, `get_deep_value` also added zero for this single-root text fixture; a full history query subsequently fetched all historical blocks. This says nothing about arbitrary nested/root enumeration workloads.

The shared initializer's `cfg(test)` check reads the last change for every peer in addition to the frontier read. Startup counts include this test check, rather than pretending it is absent.

## Where old merge pays

Instrumentation uses existing non-loading counters. Immediately before old merge, parsed DAG nodes are 2. At H1024, merge raises them to 2,051; `root_history_scan_count` remains **0 before and after**, in both arms. The whole-history work in this text case is therefore not the root-name optimization guard. The measured restoration of the old causal history is consistent with `AppDag::ensure_lazy_load_node`, which loads the unparsed suffix for a peer down to a requested old counter, and subsequent general history/diff traversal.

Repeated block reads are real in this adapter: approximately three times total stored historical bytes by completion of old merge. Do not treat the roughly seconds-scale unoptimized timing as a product forecast. `DiskKv::scan` deliberately rebuilds/filters a key vector and has no body cache; repeated range selection adds prototype overhead. The reliable architectural evidence is the set and number of fetched blocks, preserved identities, and independent convergence, not an absolute performance target.

## Correctness and failure checks executed

- Six configurations: separate author/read/enumerate processes, all passed. An additional multi-H8 smoke passed.
- Full ordinary snapshot oracle is loaded only after measured stages. Deep state, VV/frontiers and exported operation identities match. Reverse merge order also matches.
- Full snapshot export and ordinary reopen match. This export may materialize history; it does **not** establish bounded incremental C2 persistence.
- Multi-H1024 fault probe: unavailable historical reads raise a dedicated `StorageFault`, caught only around a disposable CoW candidate. Candidate is discarded; accepted owner's visible state and VV remain unchanged. Correct backing permits retry and full-oracle equality.
- Actual missing and checksum-corrupt historical files, plus missing/corrupt VV metadata, are refused. The adapter never returns `None` to pretend an unavailable referenced object is absent history. This is a test-only panic boundary, not a new production fallible API or proof of candidate usability after failure.
- Existing regressions: `import_atomicity` 17 passed; `encoding::fast_snapshot::tests` 7 passed (includes truncated block test); `cow` unit filter 9 passed. The separate `native_cow` filter matched zero tests and is **not** counted as coverage.
- Non-test `cargo check -p loro-internal --lib --locked` passed, confirming the ordinary import path compiles with `cfg(test)=false`.
- No GUI, fuzz, full suite, physical power-loss or adversarial authentication testing claimed.

## Next architectural choice

The useful split is now concrete: load current editable state plus full causal/key metadata, and keep old block bodies available but unread until needed. First reduce/structure eager metadata and design a real fallible immutable backing/overlay persistence boundary. Treat old concurrent merge as allowed to demand more history, with explicit scheduling and atomic publication, rather than requiring local opening to pay that worst-case cost. Optimizing old merge needs causal/dependency indexing and read-cache evidence; simply replacing the database will not remove the observed full-history dependency work.

## Reproduction and evidence

Raw stage rows and aggregate counters are summarized in `summary.json`. The ignored test is `encoding::fast_snapshot::c2_disk_tests::c2_trusted_disk_history_process` in `loro-internal` (default features, `--lib`, `--locked`). Set `C2_DISK_DIR`, run `C2_DISK_PHASE=author` with `C2_DISK_H=0|64|1024` and `C2_DISK_MODE=single|multi`; then run `read`, `enumerate`, and (for a multi-block fixture) `fault` in separate processes with `C2_DISK_OUTPUT` naming JSON output. Build profile was unoptimized test with debuginfo. Source and control use the same provider revision.
