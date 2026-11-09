# Immutable LMDB lease architectural control

Unmodified heed 0.22.1 / LMDB, with the existing `immutable-page-lease` public
experiment interface. This is an architecture experiment, not a production store.

## Ownership boundary

`put_reserved` initializes the payload directly in LMDB's reservation. The write
transaction commits before `bind` opens a genuinely owned `RoTxn<'static,
WithoutTls>`. **These are two different phases and addresses may differ.** No
pointer from the reservation survives commit. `producer_rebindings` counts this
boundary, even if two numeric addresses happen to match. LMDB's internal write /
page-cache copies are not counted as owner copies and must not be described as
zero-copy production-to-disk.

The bound `BytesSlice` owns the committed read epoch and environment. A Mutex
contains heed's Send-but-not-Sync transaction; it is never reset, modified, or
called after binding. Concurrent consumers read immutable memory only. Dropping
the final owner releases the epoch. Retained clones and cache hits have identical
committed addresses. The cache holds Weak references, so it does not itself pin
read epochs. Reopening does not preserve process virtual addresses.

`Descriptor.page` exists for API compatibility, but is a **logical object ID**,
not a page number or address. Store identity + object ID + length are checked;
public metadata and its required descriptors are checked in the same write txn.
The API exposes no page allocator, page refcount, or physical lifecycle controls.

## Deliberate limits and costs

* Fixed 512 MiB map; resizing is not allowed while pointers exist. MapFull is an
  error rather than an implicit remap.
* One live read slot per distinct bound object, maximum 32768. This is not a
  production epoch-batching design. A future batch could bind many objects to
  one owned transaction but would retain the same whole-snapshot recycling cost.
* A live epoch can keep **unrelated** overwritten pages from reuse. Diagnostic
  mutable-key churn plus `diagnostic_last_page_number` measures this directly;
  file high-water need not shrink when owners are released, but growth should
  cease once pages can be recycled.
* Immutable payload IDs, derived IDs, and publication IDs are never reused;
  garbage collection and cross-process coordination beyond LMDB + cooperative
  sidecar lock are not implemented. `unsafe open` requires the caller to exclude
  noncooperating mutation/truncation/unlink/resize for every outstanding lease.
* Text payload is constrained to 1..4064 bytes to match the existing page-lease
  control. This is a comparison limit, not an inherent LMDB value size limit.
* `initialized_page_bytes` retains the old metric name but counts payload bytes
  explicitly zeroed before generation, not all LMDB internal initialized pages.
* Derived metadata reads still allocate Vec; this does not claim a zero-copy
  metadata graph. The shared body path is the experimental target.

Derived head publication validates dependencies and monotonic sequence in its
write transaction. Failed generation aborts via transaction Drop; private
commits are not public until the separate publication transaction succeeds.

## Second arm: `--features detached-objects`

This opt-in format stores only object identities/index/public records in LMDB.
Text is held in one immutable file per extent, mapped by an Arc-owned read-only
mapping. It does **not** retain an LMDB read transaction. The default pinned-LMDB
arm remains separately selectable; a persistent storage-mode marker rejects
opening one format with the other feature.

Generation writes directly into a fresh create_new file's writable mapping.
The file mapping is flushed, file data/length are synced, and the objects
directory is synced **before** the LMDB index commit. The writable mapping is
made read-only without copying; the initial producer and committed owner can
therefore share the same address. No writable handle is retained. Public record
publication checks the index and the referenced file's existence/length before
commit. Cold bind checks length, UTF-8 and checksum and drops its LMDB read
transaction before returning an owner. The checksum detects accidental damage;
it is not a cryptographic authenticity guarantee.

A crash or failed generation before index commit can leave an unreachable file.
It cannot create a reference to a file that has not first passed the durability
steps. This is an ordering design, not proof of power-loss behavior on every
filesystem. Recovery/GC of orphan files, packing many objects into segments,
compaction, and independent reclamation of deleted live objects are **not
implemented**. One file and mapping per small extent can have substantial
filesystem/VMA overhead. No conclusion about total production cost follows from
text-pointer sharing alone.

The sidecar lock remains alive through the mapping owner. The unsafe-open
contract now covers the object directory/files too: uncoordinated external
mutation/truncation is forbidden for the entire owner lifetime. A read-only mmap
is not an OS-enforced seal against other writable opens. There is no claim of
safe mutable-file sharing with arbitrary writers.

The detached arm avoids pinning unrelated LMDB pages, but this does not amount
to having solved object garbage collection. It separates text-object lifetime
from database-page lifetime using ordinary immutable files, not custom database
nodes, page references, or a database allocator.

Cold publication also validates the file checksum/UTF-8 when no live validated
owner exists; a live owner is reused as the validation witness. This can incur
a body scan but not a heap-owned body copy. Path handling is absolute after
opening. The environment's immediate parent directory is synced during open;
callers creating a previously nonexistent chain of parent directories must
establish durability of that chain themselves (tests use an existing temp root).
