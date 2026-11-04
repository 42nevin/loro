# Stored Update reader memory experiment

Measured 2026-11-04 on the same Linux cloud executor as the codec experiment. Runtime provider remains d50c7c08fc37de6a07c38dfa79c1f56b85759669; this adds only an example and runner, not production changes. No new latency claims.

## Fixture, boundaries and equivalence

A separate prep process generates the fixed ordinary Update: 80 blocks, 46,272 native atomic operations, 10,037,798 bytes. Each measured child only reads these existing files; fixture construction is absent from its address space and process high-water mark. The query is peer 77 counters 700..716. All 20 children return the same 16 changes and canonical result bytes (xxhash32=2646356057), checked against the prep oracle. Original input ownership and returned results remain live until sampling finishes.

- full: the existing full native decoder plus conversion of all changes to JSON, filtering to the selected span, then canonical JSON consumption.
- full_compact: same pipeline followed by shrink_to_fit on the selected result Vec. A sensitivity control, not a changed primary baseline.
- r_cold: move the preloaded original Vec into a new reader, validate/index, query the same span, consume equal canonical JSON. No input clone.
- r_warm: reader construction occurs before the allocation scope; its memory is included in baseline requested-live bytes and whole-process RSS. Query and canonical consumption remain inside the scope.

Input bytes and expected result bytes are preloaded before the allocation scope in every arm. Both full arms retain the original input Vec; R retains that same input through the reader. Selected results and canonical consumer output are retained in all arms. The global allocator always tracks live requested bytes, even outside the scoped counters, so freeing old allocations cannot cause underflow or artificial reductions. Successful realloc changes the live ledger by new size minus old size; failure leaves it unchanged.

## Requested-heap results

Each arm ran in five fresh processes. All allocation figures below were identical across its five runs. Values are bytes unless stated otherwise.

| Arm | Baseline live | Absolute live peak | Scoped peak growth | Retained live | Retained delta | Selected Vec capacity |
|---|---:|---:|---:|---:|---:|---:|
| full | 10,045,486 | 49,578,826 | 39,533,340 | 14,873,806 | 4,828,320 | 46,272 |
| full_compact | 10,045,494 | 49,578,834 | 39,533,340 | 10,063,190 | 17,696 | 16 |
| r_cold | 10,045,488 | 10,352,980 | 307,492 | 10,068,672 | 23,184 | 16 |
| r_warm | 10,050,976 | 10,352,980 | 302,004 | 10,068,672 | 17,696 | 16 |

Full scoped growth is 37.70 MiB versus 0.293 MiB cold R and 0.288 MiB warm R. These exclude the already resident approximately 9.58 MiB input/baseline. Absolute requested-live peaks are reported separately; the entire process does not use only 0.3 MiB.

Compaction reduces the full result Vec capacity 46,272→16 and retained requested delta 4,828,320→17,696 bytes, but does not change its 39,533,340-byte scoped peak. The peak difference is therefore not solely the returned Vec retaining excessive capacity. This remains a comparison of decode+JSON+selection consumers, not isolated low-level decoders.

| Arm | Successful alloc calls | Zeroed calls | Realloc calls | Dealloc calls | Cumulative requested alloc+realloc bytes |
|---|---:|---:|---:|---:|---:|
| full | 415,867 | 0 | 841 | 415,768 | 75,075,468 |
| full_compact | 415,867 | 0 | 842 | 415,768 | 75,077,132 |
| r_cold | 2,835 | 0 | 220 | 2,733 | 3,193,529 |
| r_warm | 2,272 | 0 | 50 | 2,173 | 360,809 |

Cumulative bytes count each successful allocation request and the full new size of each successful realloc request, not physical copies or net retained bytes. Requested-live accounting does not include allocator headers/rounding/fragmentation, mappings, stacks, or System.realloc temporary old+new physical coexistence. It must not be called physical heap peak.

## Fresh-process RSS observations

All numbers in the following table are KiB, converting Linux /proc kB as 1024 bytes. Each list preserves the five run values. VmHWM is whole-process high-water RSS sampled with input/results retained, before JSON report printing, including executable/runtime startup, file loading, warm-index setup where applicable, and small measurement-observer overhead. It excludes fixture generation because prep is a different process. HWM-before subtraction is NOT used as an isolated operation peak. This is not final process-exit rusage and excludes any later reporting allocations.

| Arm | RSS before scope, five values | RSS with result retained, five values | Whole-process VmHWM, five values |
|---|---|---|---|
| full | [11316, 11376, 11392, 11492, 11492] | [47432, 47552, 47380, 47668, 47672] | [54548, 54732, 54624, 54720, 54852] |
| full_compact | [11320, 11464, 11396, 11324, 11460] | [47432, 47388, 47384, 47376, 47384] | [54484, 54644, 54620, 54492, 54576] |
| r_cold | [11436, 11392, 11544, 11540, 11540] | [13816, 13516, 13668, 13740, 13736] | [13816, 13516, 13668, 13740, 13736] |
| r_warm | [12620, 12592, 12604, 12452, 12608] | [13748, 13656, 13668, 13452, 13672] | [13748, 13656, 13668, 13452, 13672] |

Whole-process HWM medians: full 53.44 MiB, full_compact 53.30 MiB, cold R 13.41 MiB, warm R 13.35 MiB. RSS remains much higher than live requested heap after temporary allocations are freed, consistent with allocator-retained pages and other mappings; this experiment does not attribute RSS to individual components. Five runs describe this experiment only; they are not a tail-memory distribution or a general bound. Full correctness-gate compilation ran independently during this memory-only exercise; old latency results remain frozen.

## Reproduce

```sh
LORO_MEMORY_ARTIFACTS=/tmp/loro-memory-fresh bash scripts/check-update-reuse-memory.sh
```

The runner builds the release example using Rust 1.90.0, generates fixtures in a separate process, and launches five fresh children per arm. It refuses to reuse an output location containing fixture/update.loro or observations.jsonl. Standard CARGO_TARGET_DIR can reuse an existing target cache. The runner resolves the executable from the current Cargo compiler-artifact JSON, including configured target triples, and builds from the Loro checkout. No /usr/bin/time, new dependency or production-code edit is required. The allocator is active only in this example executable. No counter-instrumented timing is reused here.

Raw evidence in the companion experiment records: final-observations.jsonl (authoritative 20 runs), fixture/fixture.json, source.sha256, fixture.sha256, prep.log, reproduction.log and reproduction/observations.jsonl. The final runner self-check adds reproduction-final.log and reproduction-final/observations.jsonl; all 20 additional runs passed, with identical scoped allocation counts, cumulative bytes, peak growth, retained delta and result bytes. The script uses longer absolute command-line paths; its absolute live baseline/peak/end are each 137 bytes higher, consistent with retained argument strings. The earlier observations.jsonl is a 15-run pilot without the compaction/capacity diagnostic and is not the final table. Large generated update.loro fixture files need not be checked into Git.

Source fingerprints:

- crates/loro/examples/update_reuse_memory.rs: `6298bd68daccb11ba3d33ba6d62462bf4142d61114af9908d905f701c9fcbeb7`
- scripts/check-update-reuse-memory.sh: `da0ce9dccbb1f77bde59577011ad66c24c73db5df81d4899b66a549b15241c5e`

This does not measure E memory, SQLite, real 80-byte application Updates, edit/replay/restart state, or application peak-memory improvement. Those cannot be inferred from this 10 MB multi-block synthetic fixture.
