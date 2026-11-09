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
