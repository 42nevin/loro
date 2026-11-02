---
"loro-crdt": patch
---

Reduce native candidate fork and first-write metadata copies with persistent,
owner-isolated primary structures. Preserve default generic BTree semantics,
stable native identities, import rollback, private lazy caches and ordered
explicit enumeration. Completed SSTable metadata can share immutable ownership
without sharing decoded-block caches. Also correct an existing detached DAG
slice dependency-cutoff overshoot.

This experimental provider does not claim that all history queries, KV cloning,
application edits, synchronization or recovery are constant-time. See
`context/native-cow-string-metadata.md` for scope and qualification boundaries.
