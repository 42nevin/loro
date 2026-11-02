# Native candidate ownership and retained-history work

Verified against code 2026-11-02.

This experimental provider extends `LoroDoc::fork_cow` from base
`5f7671263f99313fd1274d9887f583aa0ca22b0c`. A candidate has separate mutable
owners, configuration, locks, lazy cells and rollback state. Immutable primary
payload and metadata may share persistent roots. There is no second resident
history mirror, snapshot fallback, or history compaction at the fork boundary.
The accepted document is not replaced by a candidate as part of these changes.

This is a collection of qualified ownership improvements, **not** a claim that
all native operations or a complete application edit are constant-time. Explicit
history export, arbitrary historical causal queries, full-state enumeration and
failure rollback can still traverse their requested inputs. Application adoption
and its durable acceptance/Unknown behavior require separate qualification.

## String metadata

`arena/str_arena.rs` stores the Unicode-to-byte index and sealed-segment
boundaries in primary `im::Vector` roots. Each owner retains a private append
tail. Segment reads seek the requested absolute offset rather than scanning the
historical prefix. Rollback seeks the retained boundary, truncates the owner's
roots and shortens only its last surviving immutable slice. Older owners retain
their roots and bytes. Small inline vectors and touched leaves can copy a bounded
chunk; zero whole-vector copies does not mean zero element copies.

Owning controls cover Unicode boundaries, 129/1025 sealed segments, retained
owners and rollback. Those cases measured zero Index/Segment clones at fork,
64/64 during rollback, and logarithmic boundary visits. Wire bytes and native
operation identities are unchanged.

## DAG metadata

`oplog/loro_dag.rs` uses a persistent primary node map. Nodes carry an immutable
optional pre-node version vector, not a shared mutable `OnceCell`. Structural
writes and computed vectors replace only the owner's path/node. `ensure_vv_for`
returns the computed vector; callers do not rely on a previously captured handle
being mutated. Only a matching causal start (peer, counter, Lamport and
dependencies) can receive a computed vector in the primary map. Detached slices
and pending nodes use invocation-local work without inserting another canonical
node. Existing known vectors survive branching: there is no empty-cache reset
substituting historical DFS for the removed map clone. An unresolved historical
query can still require ancestry traversal.

A strengthened detached-slice test exposed an existing baseline defect: merging
an unsplit containing node's end overshot the exact dependency ID. The same test
failed on the unmodified provider. The bounded correction unions through the
actual dependency ID with `extend_to_include_last_id`, preserving greater
coverage contributed by another dependency. Exact-slice, redundant-dependency,
stale-handle, diamond, shallow-history and import-rollback controls remain.

## Richtext high-water storage

`generic-btree` retains unrestricted `BTree<B, Owned>` as its default, including
non-Clone elements, deep-clone behavior and existing Send semantics. The separate
`CowBTree` mode is an explicit contract covering immutability through shared
references of **both** elements and caches. Only audited richtext and style-range
trees opt in. Their primary arena has persistent page references and 64-slot
pages, stable generational IDs and the original free-list behavior. Mutation
detaches affected pages; `get2_mut` preserves disjoint references without new
unsafe code. Clone panic, source/child mutation, cursors, undo and retained holes
are covered. Current style-map payload within a touched page can still be large.

The maintained `loro-thunderdome` copy preserves the upstream arena algorithm;
its opt-in observer measures actual full backing-vector clones. Its original
archive SHA and licenses are recorded in `UPSTREAM-PROVENANCE.json`. It is not
silently substituted with shared pages for arbitrary generic callers.

A warm native history that inserted in reverse order and then deleted back to
fixed text exposed 1024 vacant leaf slots. After the richtext change, the measured
first mutation copied two leaf pages (66 initialized slots), one internal page
(64 slots), and 21 page references, with no full backing-vector clone. Cold
snapshot compaction was a separate control, not the remedy. Explicit BTree checks
and diagnostics may still enumerate initialized storage.

## Parsed change blocks

`oplog/change_store.rs` uses a persistent primary block map and counted private
`BlockRef` handles. Completed block headers are immutable `Arc` values containing
optional immutable key/container arrays. Parsing enriches a private header and
publishes `Both` plus its arena extent only after complete decoding and container
registration. The original timestamp/message, keys, positions and container
validation order is preserved. Failed parsing does not publish a partial header;
existing arena rollback still handles its allocation side effects. Retained
`Bytes`, `Changes`, `Both` references and sibling owners remain independent.

Fork no longer clones every block wrapper and resets every header. A touched
block can still clone its header arrays; this is not a fixed 4 KiB bound. The
2/2/2/12-block two-peer fixture now reports no full block-map copy, while actual
path-reference and selected block/header copies are reported separately.

## Shared arena primary tables

`arena.rs` uses persistent ID, depth, root and value vectors and persistent
ID/parent maps. Locks and resolvers retain their distinct ownership rules:
legacy `fork` keeps its original string/resolver semantics, while `fork_cow`
installs private string state and clears source-bound resolvers. Container IDs
are never renumbered or freed by rollback. Failure rollback still filters invalid
normal parent links, invalidates depths and truncates only values/text.

Contiguous callback APIs preserve signatures, bounds checks and lock lifetime.
`with_idx_to_id` explicitly gathers its requested complete ID output;
`with_values` gathers only the requested range. These are not implicit fork
copies. Indexed value-range equality does not gather a historical prefix.
Actual ID/value leaf clones are counted; hash-map, primitive depth and root path
copies are not fully instrumented. Hash collisions also prevent a universal
worst-case logarithmic claim for the Fx-hashed maps.

## Container slots and lazy ownership

`state/container_store/inner_store.rs` stores occupied ordinal slots in a primary
persistent hash map. A slot holds an immutable wrapper plus a retained owner tag.
The selected map path detaches before testing wrapper uniqueness. A foreign tag
always rebinds through the existing `fork_cow(current_config)`, even if dropping
the source made its Arc unique. Same-owner detachment instead preserves lazy
values, private copied cells, cache-queue bits and richtext version/configuration.
It uses the existing richtext Clone rather than the version-resetting fork.

Read/decode/state access passes through this ownership boundary. Narrow metadata
reads do not expose a `State`. Explicit whole-store operations gather disjoint
references and sort by the actual ordinal before state access, preserving the
old vector order. A partially consumed all-state iterator now pays this eager
reference gather/sort, although wrapper detachment remains per yield. This cost
is explicit enumeration, not ordinary point lookup. Point insertion creates one
occupied row rather than initializing every lower historical index. The real
late-root cold-edit fixture changed from 3/19/131 initialized dense slots to one
occupied insertion and zero enumeration at each history size.

Fork also resets only the existing bounded ordinary decoded-value cache in the
child: at most 2048 queued entries in production (16 in tests), with stale entries
skipped. Reset retains encoded wrapper presence and load-state flags, so it does
not force a later full KV scan. This is bounded current-cache work, not zero work.
Normal cache-producing read APIs must use the tracked read boundary. Arbitrary
crate-internal raw mutable-wrapper enumeration is not covered by that producer
invariant. Repeated fork/disjoint ordinary reads, held owners, queue bounds,
source version, source-drop configuration binding and malformed cold reads are
owning controls. Existing Send/Sync tests remain; compiler recursion limit 256
allows the deeper finite persistent type proof without manual unsafe traits.

## KV metadata and remaining costs

Completed `SsTable` metadata is an immutable `Arc<[BlockMeta]>`, constructed only
at the existing successful builder/import boundary after unchanged validation.
Each cloned table still creates its own bounded mutable decoded-block cache.
Builder metadata remains a Vec; codec, checksum and decode timing are unchanged.
Construction can allocate/move metadata once; zero metadata Clone calls is not
zero allocator work.

The final observer measured two KV store/table clones and two fresh caches in
ordinary fixture forks, memtable entries zero warm/one cold, and metadata growth
from two to three entries in a retired-container history. The finite metadata
sharing change removed those BlockMeta clones. **MemKvStore's BTreeMap clone and
its table-wrapper vector clone remain.** Generic multiple-table imports are not
proven constant-time, and no export/compaction was inserted to shrink them.

## Measurement and qualification boundaries

`cow_work` is opt-in per thread under test/test_utils. It reports actual clone
sites/path copies, sparse point insertions, explicit enumeration, bounded cache
resets and dependency-level KV/arena work. Entries are not payload bytes, RSS or
a CPU bound. Counters record attempted operations if a clone panics. Removed
bulk-operation counters do not certify uninstrumented work as zero.

Every optimized layer had a genuine behavioral counter RED before its runtime
change. Final native gates include the five fixed-input phase fixtures, fork and
import atomicity controls, KV tests and the internal suite (434 passed, four
ignored). These native outcomes do not replace the separate application
feature/default comparison, durability tests or runtime measurements. Sync,
arbitrary-DAG proof reconstruction and cold recovery are not solved here.
