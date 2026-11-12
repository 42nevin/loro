# Live immutable extent relocation

Verified against code 2026-11-12.

The independent `experiments/immutable-page-lease` provider uses payload schema
v3. `Store::open` rejects v2 and unmarked stores before mutable transactions;
there is no migration. The 32-byte slot header and 4064-byte payload limit stay
unchanged. `Commitment` is the journal-safe identity (store UUID, logical ID,
length, BLAKE3 digest); `Descriptor` remains a strict physical diagnostic.

`store/relocation.rs` (source file `src/relocation.rs`, included under `store`)
resolves commitments through the mutable root-0 directory. Root 6 stores the
five immutable fields using a fixed `repr(C)` `(id, field)` key. Every disk
resolution validates UTF-8, slot ID, length and digest. A live cache hit trusts
an already validated owner under `Store::open`'s external-mutation exclusion.
IDs with durable commitments cannot be reissued, even after test-only sweeps.

`Store::relocate` allocates a new payload page, copies the original immutable
bytes, and commits the directory switch and root-7 retired-page queue in one
Sanakirja transaction. It neither edits public/derived records nor changes
logical IDs. `publish_committed` authenticates stable commitments before opaque
record publication; physical `bind` continues to reject stale placement.

Every displaced owner is retained as a Weak entry with its old page. Repeated
moves preserve all generations, not just the last cache entry. Reclamation
requires no current directory slot on the page and no surviving retired owner.
Packed neighbors block a page until they also move; a dropped lease cannot be
resurrected because retired weak pointers are private. Exclusive `&mut Store`
excludes concurrent new binds while checking/reclaiming. Owner-held mappings
and inode/path exclusion survive Store drop and cross-thread lease drops.

`reclaim_relocated` only frees retired payload pages, atomically removing queue
entries. It does not collect history, authority, logical identities, public
records, derived objects or abandoned current payloads. Root 7 remains present
after first use; legacy test-only sweeps refuse that store, including when the
queue becomes empty. These sweeps are not application GC.

A reported commit error fail-stops subsequent writer operations and payload
resolution until reopen, so potentially stale runtime cache entries are not
trusted. Unit fault injection simulates abort before publication and successful
commit with lost acknowledgment. It is not process-kill or power-loss evidence.
The experiment relies on the pinned engine's commit/recovery behavior; hardware
failure, torn writes and external corruption during live leases are outside the
proof. Tests corrupt same-length payload bytes only after closing all owners,
then verify reopened commitment resolution rejects them.

Tests: `tests/live_relocation.rs` and `src/relocation.rs` cover repeated moves,
old owners, packed neighbors, cross-thread drops, reopened stable commitments,
physical reuse, unchanged journal blob bytes, preserved derived records,
legacy sweep refusal and both deterministic transaction-boundary injections.

## Pinned engine durability audit

Sanakirja 1.4.3 `environment/muttxn.rs`, `Commit for MutTxn<E, ()>`
(lines 243–389 in the registry source), first builds the free-page tree and
clears dirty bits. At lines 334–337 it flushes every mmap. Only afterward does
it set the target root page's length/free/RC roots and application roots
(including directory 0 and retirement queue 7), set the optional CRC, and
advance the global root selector. At line 379 it synchronously flushes the root
page range before unlocking and returning. `Map::flush`/`flush_range` delegate
to memmap2, whose Unix implementation calls `msync(..., MS_SYNC)`. Thus newly
allocated payload/directory/queue pages are flushed before root publication;
reclamation's free-tree update and queue removal use the same commit path.

The provider enables the engine's default mmap feature, **not crc32**. The
engine's root CRC hooks are no-ops in this configuration. Opening a transaction
selects the root preceding the global next-writer selector; this wrapper does
not implement fallback root scanning or torn-root recovery. The final root-page
flush is not evidence that multiple root fields/selector updates are physically
atomic against power loss. We have not tested kills *inside* the engine commit,
filesystem/device failure, torn writes, or hardware cache behavior. Therefore
“atomic publication” here denotes the engine transaction API and tested
boundary recovery, not a proved power-loss-safe storage protocol.

`sigkill_before_and_after_publication_recovers_mapping_and_queue` starts a real
subprocess, waits for an explicit barrier (all relocation changes staged before
commit, or commit returned but runtime cache not updated), and sends SIGKILL.
Reopen must select the old mapping with no retired page before publication, or
the new mapping with the old page durably reclaimable after publication. No
child transaction or Store destructor executes. This complements the returned
error simulations but still does not simulate hardware power loss.

Every new commitment read and retired-queue scan increments the provider's
read-transaction metric. A commitment-bound cache hit therefore avoids payload
resolution, but still accounts for its immutable commitment-tree lookup.
