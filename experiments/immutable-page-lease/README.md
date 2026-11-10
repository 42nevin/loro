# Immutable Sanakirja page lease qualification

This independent crate tests an alternative to the reservation-to-`BytesSlice`
ownership copy. It is **not** the full Gestio A–D comparison and does not replace
its receipt, publication, fault, or performance qualification.

## Model

`Store::insert` allocates one Sanakirja page, initializes its payload in the
allocation callback, validates UTF-8, records its logical ID in a B-tree, and
commits. Only after commit does it create an external immutable `BytesSlice`
owner. The owner retains `Arc<Env>` and the payload address. No database read or
write transaction remains open in the owner. Native Loro retains that owner in
its arena, operation and rich-text structures.

The wrapper never mutates a committed payload and never passes its page to
`decr_rc`, `free_page`, or another reclamation path. Page offsets are stored as
plain `u64` metadata values, not as automatically reference-counted page objects.
Metadata B-tree pages can be replaced/reclaimed normally. Payload pages remain
allocated forever, including orphaned/abandoned ones. This deliberately trades
space for a simple qualification invariant. It is not a production GC design.

## Safety boundary

`Store::open` is unsafe. The caller must exclusively control the file for the
entire lifetime of **every** derived owner, not merely the Store. No second
mapping, outside writer, truncation, replacement, deletion, compaction or page
reclamation is permitted. Existing stores must be products of this wrapper.
Store does not expose its environment or a raw transaction. The tests satisfy
this boundary using private temporary files and drop all documents and leases
before reopening.

The `ImmutableBytes` implementation is an audited unsafe boundary, not a
`transmute` of a transaction borrow. It establishes a stronger owner invariant:
committed immutable pages remain allocated and mapped until the final owner
dies. In pinned Sanakirja 1.4.3, `Env::find_offset` appends new mappings on growth
rather than replacing existing mappings (`src/environment/mod.rs:344–365`).
Mappings are retained in the environment. Allocation returns fresh page storage;
reclamation is explicit (`environment/muttxn.rs:420–516,625–670`). The page's
first eight bytes are reserved for Sanakirja bookkeeping, and payload begins at
byte 32. Commit may modify bookkeeping before any owner escapes. An error or
panic before commit never publishes a lease.

This contract is specific to pinned Sanakirja 1.4.3/core 1.4.1 and must be audited
again before upgrading. Tests are evidence, not a substitute for these invariants.

## Coverage and accounting

- Same payload address at generation and after transaction completion.
- 179 later page writes force file/mapping growth while old bytes remain valid.
- Owner remains readable after Store drop, including on another thread.
- All owners drop, then reopening resolves payload by logical ID.
- Loro G1 `a`, abandoned CoW branch, reopen, G2 `玉`, historical cut and native
  incremental sync with exactly one missing logical atom.
- Invalid UTF-8 and generator panic publish no committed extent/lease.

Ownership-transfer payload copy count is zero, corroborated by comparing the
address inside the Loro generator with the address of the committed payload.
The Loro generator encodes Unicode scalars directly into that page. Every extent
zero-initializes all 4096 bytes, counted separately as initialized_page_bytes.
The generator still writes payload bytes. The Loro native export/import recovery
route materializes wire bytes and reconstructed native owners; this prototype
makes no zero-copy claim about decoding: reopening Loro through a native snapshot
copies the old prefix. Reopening the payload via Store::get is independently
zero-copy; this is not database-native Loro rehydration. Cross-extent joins, public string
materialization and native encoding remain ordinary Loro costs. There are no
performance rankings here. Physical/durable sync behavior is Sanakirja's normal
commit, not a measured full Gestum latency result. No SIGKILL or power-loss result
is established by these unit tests.

Each extent is nonempty and at most 4064 bytes; larger text must split on UTF-8
boundaries. Every text insertion currently commits immediately. There is no
bounded idle group-commit mechanism, shared multi-scope publication transaction,
durable private-draft visibility policy, canonical receipt or atom directory in
this standalone prototype. The existing qualification experiment supplies those
separate concerns.

## Run

From the Loro checkout:

```
bash experiments/immutable-page-lease/reproduce.sh
```

The script uses Rust 1.97.1 and the committed lockfile. Install that toolchain
first; the first run downloads dependencies. Set CARGO_NET_OFFLINE=true only
when the dependency cache is already populated. The saved qualification logs
were produced with the equivalent explicit --offline flag. Results are retained in
`results/`. An expanded pre-fix test failure on Rust 1.90.0 is retained there too:
it exposed a logical ID lookup bug because Sanakirja get is a lower-bound lookup,
not an exact match. The wrapper now compares the returned key explicitly. The local
vendored `append-only-bytes` patch must remain the same crate instance used by
Loro and this prototype; otherwise the allocator return types differ.

## S-direct extension (2026-11-07)

The current source additionally exposes `insert_bound`, `bind`, `publish`,
`published` and `published_ids`. Private payload descriptors and public record
metadata share one environment. Store identity persists across reopen. A public
record is installed atomically only after every descriptor matches an existing
private page; duplicate public keys are explicitly rejected. Application receipt
and semantic verification remain the caller's responsibility. Metadata blobs are
also retained forever. Initialization adds one store-identity commit; each edit
uses a private commit and then a public commit, counted separately.

The generated allocator now returns `TextSlice::Direct`, retaining logical extent
identity alongside the immutable owner. Diagnostic actual OpLog handles confirm
the original allocation address. The original snapshot-based test remains a
legacy test, not proof of direct fresh-process Loro recovery. Application tests
must bind records from Sanakirja, not import a snapshot and call that direct.

`Store::metrics` records actual extent resolutions, read transactions, private
commits, public commits and initialized page bytes. Compare snapshots around the
live accessor loop; the counters are shared by all threads using the owner.
`TextSlice` separately records explicit payload copies by consumer. Counters do
not claim to cover every allocator or wire serialization operation.

The wrapper now retains cooperative path and inode exclusion through all leases.
On Linux the inode lock is an OFD record lock, separate from Sanakirja's own flock
write lock. Existing aliases are rejected before mapping. Arbitrary external
writers and file replacement are still excluded by the unsafe caller contract.
See `context/s-direct-page-ownership.md` for the pinned engine source audit.

## Whole-page reclamation probe (2026-11-10)

`src/reclamation_probe.rs` is a test-only exception to the public append-only
policy. It retires whole unrooted, unleased pages in a closed-world unpublished
fixture and proves allocator reuse, including old editing branches and
cross-thread handles. It refuses opaque public/derived roots after reopen.
It also reproduces a stale-descriptor ABA counterexample after retired IDs are
reused. **There is no production GC API and the identity hazard is not fixed.**

Run `bash experiments/immutable-page-lease/reproduce-reclamation.sh` from the
checkout, optionally with `CARGO_NET_OFFLINE=true` after dependencies are cached.
See [the experiment and architectural gates](../../context/page-reclamation-experiment.md)
for measurements, retained-history limits, required durable root/ID policy and
unverified crash boundaries. Allocator reuse does not shrink the file or return
filesystem blocks.

## Durable roots and identities model (2026-11-10)

A second private test-only controller in `src/rooted_reclamation.rs` persists
versioned root/dependency manifests, permanent identity high-water marks and root
retirement tombstones. Unread lazy-root pins and byte leases both prevent reuse.
Four separate processes verify shared-dependency retention and root retirement;
a physical-reuse test rejects the stale descriptors accepted by the original
unprotected probe. A lost-success-acknowledgment test verifies fail-stop/reopen
behavior after actual successful commits, not engine I/O failure or crash safety.

Run `bash experiments/immutable-page-lease/reproduce-rooted-reclamation.sh`.
See [the durable-root model](../../context/rooted-reclamation-model.md) for the
protocol, evidence and limitations. Its roots are synthetic; automatic Loro root
discovery, production APIs, metadata GC and crash qualification remain unimplemented.
