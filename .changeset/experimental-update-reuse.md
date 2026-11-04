---
"loro-crdt": patch
---

Add experimental native Rust APIs for reusing complete encoded source blocks in
ordinary Update exports and reading selected native changes from trusted stored
Update bytes. Keep default exports and remote import validation unchanged. Add
opt-in thread-local work counters for independent experimental measurements.
