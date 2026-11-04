# C-ref representation and recovery qualification

Run `bash experiments/cref-validation/reproduce.sh` from a clean checkout with
Rust 1.97.1 installed. Dependencies download on first run; set
`CARGO_NET_OFFLINE=true` only with a populated cache.

This validates the Loro half of the matched LMDB experiment, not LMDB durability
or a performance ranking. See `context/shared-arena-cref.md` and
`context/extent-descriptor-restore.md` for APIs, scoped reads, exact materialization
boundaries, typed recovery, matched cache policy, and read-failure limitations.
The separate rkmki paired harness supplies real LMDB transactions, reference
publication, process-crash tests, fresh-process recovery and timing samples.

The results directory retains passing runs and earlier failed qualification
samples. The peer-ID oracle failure was a test helper misreading compressed JSON
peer identifiers, corrected before validation. The read-poison trace is a real
limitation: a legacy infallible read error panics and poisons the speculative
document, whose normal Drop may also panic. The final test catches execution and
cleanup separately and verifies the source is unchanged; it does not claim
arbitrary storage I/O errors are production-recoverable inside Loro.

Metadata-only `inspect_text_backings` diagnostics are correctness scans outside
all timings. They report actual arena/Op/live-state backing kind and logical
extent/range, not just adapter counters. Run with no concurrent document edits;
individual collections use separate locks. Unloaded container state is not
resolved merely to make diagnostics look complete.
