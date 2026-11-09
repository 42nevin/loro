---
"loro": patch
---

Avoid rebuilding the full historical key directory for each range lookup in the
opt-in trusted history experiment. Preserve overlay deletion and bidirectional
iteration semantics while reading payloads only as results are consumed.
