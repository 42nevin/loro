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
