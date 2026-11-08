---
"loro": patch
"loro-internal": patch
---

Reuse the OpLog version vector when the document state has the same frontiers,
avoiding redundant historical DAG reads; preserve the original conversion for
other frontiers. Add latest-state read-count and original-algorithm regressions.
