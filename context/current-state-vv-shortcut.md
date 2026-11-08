# Latest-state VV lookup

For a valid document whose state frontiers equal OpLog frontiers, `state_vv`
clones the OpLog VV under the existing locks. Historical/detached state retains
the original `dag.frontiers_to_vv` path. This is a version-query optimization,
not an independent adversarial-snapshot audit. No ordinary import validation
was changed. No cursor cache is involved.

Fresh recovery-era RED/GREEN evidence and limits:
[qualification](../docs/experiments/current-state-vv-redo-20261108/QUALIFICATION.md).
