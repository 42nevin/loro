# Trusted disk history experiment (test only)

Verified against code 2026-11-08. Baseline a64b750745dc26d279f98ca6b52f2c15351b4799.

`encoding/fast_snapshot/c2_disk_tests.rs` splits a fully validated ordinary
snapshot into current state and indexed historical KV objects. A new process
attaches the state plus a test-only disk KV adapter, retaining complete VV and
history (not shallow). The manifest is a trusted local fixture; per-object
xxhash32 detects accidental corruption, not malicious substitution. Ordinary
external snapshot import still imports and checksum-validates all KV blocks.

`ChangeStore::initialize_imported_metadata` is the shared existing post-import
initializer. Only `cfg(test)` hooks can attach the test backing to a fresh
isolated OpLog. CRDT merge/diff algorithms are unchanged. Local writes go to an
independent overlay; CoW clones share immutable index and backing references,
not overlay mutation. Reads and iterator consumption count actual body reads.

The infallible existing KV trait cannot express storage unavailability. The
experiment raises a dedicated `StorageFault` panic on absent or checksum-bad
referenced objects and catches it only around isolated disposable candidates.
This is not a production error API or a guarantee that a failed candidate is
usable. Accepted owner is retained and checked unchanged. No missing object is
silently treated as a missing historical operation.

The ignored `c2_trusted_disk_history_process` test supports `author`, `read`,
and `fault` phases through `C2_DISK_PHASE`, with `C2_DISK_DIR`, `C2_DISK_H`,
`C2_DISK_MODE=single|multi`, and `C2_DISK_OUTPUT`. Single mode allows coalescing;
multi mode changes peers to retain separate Changes. Actual Changes, blocks,
and bytes are recorded. It measures attach, known text access, CoW first edit,
old-peer update, deep value and full-history query before full oracle reads.
Full snapshot export/reopen is separately measured and does not establish
bounded incremental persistence. Semantic controls preserve operation IDs and
check both merge orders. Read counts are logical file reads, not cold-page I/O.

Run the focused ignored test explicitly. Regression filters required around the
shared metadata extraction remain `import_atomicity` and
`decode_updates_rejects_truncated_block`. Record actual results; this context
file does not claim those checks have passed.
