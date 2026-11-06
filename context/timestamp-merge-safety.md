# Timestamp-safe optional Change merging

The isolated Gestio B experiment preserves already-admitted native snapshot
history with decreasing timestamps. `Change::can_merge_right` uses checked
subtraction and merges only when the delta is nonnegative and within the caller's
merge interval. Negative or overflowing distances keep distinct Changes. No
IDs, operations, dependencies, timestamps, messages or Lamports are rewritten.
Ordinary equal/increasing merge conditions and other native assertions remain.

The independent actual-app genesis fixture was admitted by `read_native_archive`
with full state, version vector and membership seed unchanged. Its two Changes
have timestamps 20 then 10 and equal messages. Before this repair, direct full
JSON replay aborted at the nondecreasing assertion. App guard rejection also
prevented typed-root restoration. Native Update/snapshot-base and fork_cow suffix
probes previously aborted instead of establishing accepted Update history.
Those paths may now accept the history: comparisons must name this repaired
provider and must not call it unmodified baseline A.

Regression contract: `crates/loro/tests/timestamp_import_contract.rs`. Actual
app-admitted history and synthetic MIN/MAX typed API safety cases are separate
claims. App receipt/typed correspondence and causal/range/coverage checks remain
required; this provider predicate is not an authorization or receipt check.
Full JSON replay still does not promise exact arbitrary Change segmentation,
original physical text ownership, or complete snapshot configuration equivalence.
