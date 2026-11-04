# Ordinary Update block reuse experiment

Verified against code 2026-11-04. Baseline: 531a959f5322f9fd42f30b5de65f1047a721b498.

## Scope

The additive `LoroDoc::export_updates_reusing_blocks` and
`export_updates_in_range_reusing_blocks` APIs produce the existing FastUpdates
(mode 4) envelope and LEB128-prefixed blocks. Default export and all remote
import validation remain unchanged. Packing and bytes may differ from default
export; native IDs, dependencies and operations must be equivalent.

`ChangeStore::reuse_source_blocks` loads only source block range facts, unlike
`iter_blocks`, which forces operation decoding. Complete source blocks with
Bytes or Both content copy cached encodings. Changes-only complete blocks
encode once and retain Both content; these are NOT counted as cache hits.
Mutation already invalidates Both bytes through `changes_mut`. Boundary blocks
use native `Change::slice` and the same block codec. Overlapping/reversed input
spans are normalized and gaps remain gaps; disjoint spans never become one
non-contiguous change block. Source metadata inspection can populate block
wrappers but never operation payloads. It commits pending transactions.

## Stored Update reader

`StoredUpdateReader::new` owns the original bytes, verifies the full document
checksum, parses every length prefix, and builds a peer-to-block index using the
existing block headers. The constructor does not decode operation payloads.
Duplicate or overlapping block native ranges are explicitly rejected during
construction; ordinary remote import retains its deduplication behavior.
Queries scan the selected peer's block metadata, decode only overlapping blocks
into a fresh arena, and slice native changes. `query` returns owned remote-form
native changes; `query_json` additionally converts the same native changes to
JSON. Neither creates or imports a document, checks dependency availability,
computes frontiers, or applies state. Missing external dependencies are therefore
preserved in the returned native changes, not silently dropped as pending ops.
This reader is for already accepted trusted local stored Updates, not remote
import or an alternate validation path. Corrupt unselected payloads whose
checksum was deliberately recomputed may not be rejected until selected.

`decode_update_json` is a comparison helper over the existing full
`fast_snapshot::decode_updates` decoder and identical JSON conversion. It
checksums and decodes every block, preserves changes with external dependencies,
and never applies document state. Label measurements of it as a full native
decoder baseline, not an existing application live-document history read.

## Work and latency accounting

`update_reuse_work::start` resets and enables thread-local counters; `finish`
returns them and disables accounting. Regions are not nestable. Keep counters
disabled for latency distributions and collect work counts in separate runs.

Native block encode/decode and `Change::slice` are instrumented centrally, so
control and candidate paths share counters. `decoded_blocks/bytes` count decode
attempts; decoded changes, RLE operations and atomic operations count completed
block decodes. Encoding counts completed blocks. `selected_blocks` counts store
export selections or reader payload selections. Reader metadata scans are
separate. `copied_bytes` counts block payload bytes appended to Update outputs,
not arbitrary allocator, arena or native-to-JSON copies. The eligible-whole
counter partitions into cached reuse versus newly encoded whole blocks.
`checksum_scanned_bytes` is bytes hashed for envelope validation/construction.
`header_input_bytes` is the aggregate payload length passed to header parsing,
NOT bytes physically scanned; postcard can skip borrowed payload slices.
`index_builds/indexed_blocks` expose full reader construction. A cold read must
include construction; a retained-reader query must be labeled warm and cannot
hide the initial checksum/index cost. The peer index itself is linear-scanned
on each query and is charged in `query_metadata_scans`.

There is no claim that ordinary edits, live imports, late merges, restart replay,
or database writes are eliminated. These paths may still need complete native
payload decoding and state application. Benchmark them separately from stored
historical span reads, and include original-byte database read costs externally.
