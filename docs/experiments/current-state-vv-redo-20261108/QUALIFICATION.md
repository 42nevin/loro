# Latest-frontier state_vv: reconstructed experiment

Base: `fa12d1d4175a42798279d1f3329777a2a65097b4`, tree
`aeefb4864d9c1d5e125cc9526f26ff5c3c854d08`. The original workspace was lost;
these are newly written tests and fresh runs, not recovered old raw evidence.

## Question and change

At the latest state frontier, can `state_vv()` use the already stored OpLog VV
instead of traversing historical DAG blocks? Under the existing lock ordering,
compare state frontiers with OpLog frontiers. If equal, clone OpLog VV; otherwise
retain exactly the original DAG conversion. No cursor cache, mutable VV cache,
new snapshot format or CRDT algorithm was introduced.

For valid provider states these describe the same causal version. The existing
`DocState::init_with_states_and_version` uses this same frontier-equality choice
when obtaining VV for unknown-container reconstruction. This is not independent
causal validation of an adversarial snapshot. Ordinary import decoding and its
validation were not changed, and the optimization is not a new security boundary.

## Fresh RED then GREEN

Public trusted-history fixture: 32 insert/delete history rounds, rotating four
peer identities, final visible text `abcdef`; an immutable backing source counts
actual callback reads. Read counts refer to this fixture, not physical disk I/O.

| Query | Original fa12 | Minimal fix |
|---|---:|---:|
| Latest-state VV source reads | 4 -> 20 (+16), assertion RED | 4 -> 4 (+0) |
| CoW branch VV additional source reads | not reached after RED | 0 |

The original-DAG oracle runs only after the read-count assertions. A separate
oracle test compares `state_vv()` with `frontiers_to_vv(state_frontiers())` for
empty, committed current, uncommitted insertion/deletion, CoW current/pending/
committed, detached historical, detached after old-device import, reattached
merged latest, and shallow latest/retained historical/pending/edited latest.

The public suite also retains the fa12 capture/reopen/CoW ownership and injected
storage-failure disposal tests. Expected caught panic messages in the log are
not failed tests. Failed candidates are discarded, not repaired.

## Scope of conclusions

This removes a redundant latest-frontier history traversal in the measured
provider path. It does not prove bounded metadata, sub-document restoration,
constant-time first edit, or bounded old-device merge. Current state and full
key metadata remain eager. Application readiness must be remeasured against the
new exact provider commit; no application improvement is claimed here.

Tests run in an unoptimized native build, no GUI, fuzzing or full-suite claim.
See CHECKS.txt and accompanying fresh logs for exact test counts. Source/binary
hashes identify these runs; commit ancestry is preserved by the coordinator.
