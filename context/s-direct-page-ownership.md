# S-direct immutable page ownership experiment

Verified against code 2026-11-07. Provider baseline e886e38d.

`TextSlice::Direct { bytes, extent }` binds a logical extent ID to an immutable
`BytesSlice` owner. `direct_extent` checks UTF-8; the application must independently
authenticate the ID/owner correspondence. `direct_bytes` borrows the owned bytes,
without resolving an ID or entering a storage transaction. Byte slicing keeps the
same owner and original range. Direct slices merge only when they are adjacent
ranges of the same owner and identity; arbitrary multi-page concatenation is not
qualified here. Explicit `to_vec_for` remains counted as a payload copy.

`LoroDoc::inspect_text_op_handles` clones handles from the actual retained OpLog
insertion operands. It is a correctness diagnostic, walks history, and must stay
outside timing windows. It is not a string export or persisted-record decoder.
The existing bound JSON importer preserves a single Direct handle, passes its
clone to the arena, and installs the original in the Op. It still constructs
JSON/wire verification material; this change does not claim full-flow zero copy.

## Store ownership

`experiments/immutable-page-lease` extends the earlier 2d82c98b implementation.
`Store::insert_bound` allocates one page, generates into bytes 32.., validates
UTF-8, records its descriptor and commits before a `SealedSegment` escapes.
The segment retains `Arc<Owner>` with the Env and exclusion locks. Committed
payload and metadata blob pages are never reclaimed. They are indexed as plain
u64 offsets, not engine-recognized page references. A private orphan is retained
but is not a public Gestum. The application performs semantic acceptance and
receipt verification before `publish` atomically installs metadata under root 1.
Payload descriptors are rooted at slot 0 and the persistent store identity at 2.
Public metadata blobs can span pages; text operands remain single-page bounded.

Pinned engine audit: Sanakirja 1.4.3 `environment/mod.rs` find_offset appends mmap
chunks (lines 344–365); `environment/muttxn.rs` alloc_page_ reuses only explicitly
freed page sets (483–513). sanakirja-core 1.4.1 u64 Storable has no referenced
pages; clear_dirty changes only the first eight bookkeeping bytes. No wrapper
API exposes a transaction or payload free/mutate operation. These properties
must be reaudited before a dependency upgrade.

The Linux wrapper holds a sidecar flock for path initialization plus an OFD
record lock on the actual inode, both until the last derived segment dies.
OFD locks catch cooperating hardlink/symlink aliases and are independent of the
flock Sanakirja uses for its own write transactions. Holding a separate flock
on the actual data file instead would deadlock commits; a development test found
this and was terminated before replacing it with the OFD lock.

`open` remains unsafe: the caller excludes noncooperating processes, replacement,
truncation, corruption, compaction and fresh-creation alias races for the lifetime
of every derived segment, not just the Store facade. Advisory locks do not stop
an arbitrary writer. Existing files must be created solely by this wrapper.
This is feasibility evidence under a restrictive retention policy, not a general
safe mmap API or production garbage collector.

## Packed payload schema v2 (2026-11-09 experiment)

The provider now hard-cuts payload storage to schema v2. Identity root 2 keys
4/5 hold the payload version/magic; keys 0/1 remain the UUID and keys 2/3 remain
reserved for the independent derived schema. Existing stores without the exact
v2 marker are rejected using a read-only transaction. There is no migration or
implicit old-format admission. Serialized descriptors require a page-relative
`offset`; `page` still denotes a 4096-byte-aligned page, not a slot address.

`insert_batch_bound` takes an explicit list of identities/lengths and invokes the
caller's generator once per extent in input order, directly in newly allocated
pages. Each slot is 8-byte aligned and has 32 header bytes: eight reserved bytes,
magic, byte length, and extent identity. Root 0 indexes the page-plus-slot-header
address. The single-extent maximum remains 4064 bytes. All extents are validated
and committed in one private transaction before any lease escapes. Invalid UTF-8,
duplicate identity, or generator panic cannot expose a partial batch. A later
batch allocates fresh pages, never fills an earlier sealed page's remaining tail.
This does not batch future keystrokes or change public-record publication.

Each logical extent still has a distinct descriptor and lease, including when it
shares a physical page with another extent. A lease retains the same environment
owner and locks. No extent or page is reclaimed. The live binding cache is weak:
it reuses authenticated leases but neither extends mapping lifetime nor replaces
cold-reopen validation. A 64-by-256-byte batch uses five explicit payload pages
and one private commit; this is a structural test, not an end-to-end latency claim.

A live segment enters the weak index only after new allocation or successful
on-disk validation. Each requested descriptor is compared in full. The final
consumer removes its weak entry, so no persistent trust cache or dead-entry
accumulation is introduced. Cache misses still check index, identity, range
and UTF-8.

## Test-only reclamation qualification (2026-11-10)

The public retention guarantees above are unchanged. A closed-world test-only
probe now retires unpublished pages only after every extent on a page is absent
from explicit roots and live leases. It refuses opaque published/derived roots.
It also reproduces descriptor ABA when retired IDs are reused, so it must not be
promoted to a public collector. See [page-reclamation-experiment.md](page-reclamation-experiment.md)
for lifecycle measurements and the required durable reachability/identity gates.
