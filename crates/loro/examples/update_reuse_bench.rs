//! Codec-only benchmark, not an application/DB benchmark.
//! cargo run --release --locked -p loro --example update_reuse_bench -- 100
use loro::{
    decode_update_json,
    update_reuse_work::{self, UpdateReuseWork},
    CommitOptions, ExportMode, IdSpan, JsonChange, LoroDoc, StoredUpdateReader, VersionVector,
};
use serde_json::{json, Value};
use std::{hint::black_box, time::Instant};

const PEER: u64 = 77;
fn work_json(w: UpdateReuseWork) -> Value {
    json!({
        "selected_blocks":w.selected_blocks,"sliced_changes":w.sliced_changes,
        "encoded_blocks":w.encoded_blocks,"encoded_bytes":w.encoded_bytes,
        "encoded_changes":w.encoded_changes,"encoded_ops":w.encoded_ops,"encoded_atom_ops":w.encoded_atom_ops,
        "decoded_blocks":w.decoded_blocks,"decoded_bytes":w.decoded_bytes,
        "decoded_changes":w.decoded_changes,"decoded_ops":w.decoded_ops,"decoded_atom_ops":w.decoded_atom_ops,
        "copied_update_payload_bytes":w.copied_bytes,"eligible_whole_blocks":w.eligible_whole_blocks,
        "reused_cached_blocks":w.reused_cached_blocks,"newly_encoded_whole_blocks":w.newly_encoded_whole_blocks,
        "whole_block_cache_hit_rate":if w.eligible_whole_blocks==0 {None} else {Some(w.reused_cached_blocks as f64/w.eligible_whole_blocks as f64)},
        "checksum_scanned_bytes":w.checksum_scanned_bytes,"index_builds":w.index_builds,
        "indexed_blocks":w.indexed_blocks,"header_input_bytes":w.header_input_bytes,
        "query_metadata_scans":w.query_metadata_scans
    })
}
fn observed<T>(f: impl FnOnce() -> T) -> (T, Value) {
    update_reuse_work::start();
    let result = f();
    let work = update_reuse_work::finish();
    (result, work_json(work))
}
fn distribution(mut samples: Vec<f64>) -> Value {
    samples.sort_by(f64::total_cmp);
    let p = |q: f64| samples[((q * samples.len() as f64).ceil() as usize).saturating_sub(1)];
    json!({"n":samples.len(),"unit":"microseconds","p50":p(0.50),"p95":p(0.95),"p99":p(0.99),"min":samples[0],"max":samples[samples.len()-1]})
}
fn measure<S, T>(n: usize, mut setup: impl FnMut() -> S, mut run: impl FnMut(S) -> T) -> Value {
    // Counter regions never overlap timing. Setup and result destruction excluded.
    assert_eq!(update_reuse_work::finish(), UpdateReuseWork::default());
    for _ in 0..5 {
        let input = setup();
        black_box(run(input));
    }
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let input = setup();
        let start = Instant::now();
        let output = run(input);
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        black_box(output);
    }
    assert_eq!(update_reuse_work::finish(), UpdateReuseWork::default());
    distribution(samples)
}
fn load(snapshot: &[u8]) -> LoroDoc {
    let d = LoroDoc::new();
    d.import(snapshot).unwrap();
    d
}
fn export(d: &LoroDoc, span: IdSpan, reuse: bool) -> Vec<u8> {
    if reuse {
        d.export_updates_in_range_reusing_blocks(&[span]).unwrap()
    } else {
        d.export(ExportMode::updates_in_range(vec![span])).unwrap()
    }
}
fn full_select(bytes: &[u8], span: IdSpan) -> Vec<JsonChange> {
    // Fixture has one native map operation per committed change. This is an
    // actual full native decode plus selection, never a second indexed reader.
    decode_update_json(bytes)
        .unwrap()
        .into_iter()
        .filter(|c| {
            assert_eq!(c.op_len(), 1);
            c.id.peer == span.peer
                && c.id.counter >= span.counter.start
                && c.id.counter < span.counter.end
        })
        .collect()
}
fn main() {
    let n = std::env::args()
        .nth(1)
        .map(|s| s.parse().unwrap())
        .unwrap_or(100);
    assert!(n >= 100, "use at least 100 samples for reported p99");
    let source = LoroDoc::new();
    source.set_peer_id(PEER).unwrap();
    source.set_change_merge_interval(0);
    let map = source.get_map("history");
    let mut next = 0;
    let mut rows = Vec::new();
    for target_blocks in [5, 20, 80] {
        let blocks = loop {
            let blocks = source.update_source_blocks(&[IdSpan::new(PEER, 0, i32::MAX)]);
            if blocks.len() >= target_blocks {
                break blocks;
            }
            for _ in 0..64 {
                let i = next;
                let payload: String = (0..192)
                    .map(|j| {
                        char::from_u32(0x20 + ((i * 7919 + j * 101 + (i ^ j) * 37) % 90) as u32)
                            .unwrap()
                    })
                    .collect();
                map.insert(&format!("key-{i}"), payload).unwrap();
                source.commit_with(
                    CommitOptions::new()
                        .timestamp(i as i64)
                        .commit_msg(&format!("bench-{i}")),
                );
                next += 1;
            }
        };
        assert_eq!(blocks.len(), target_blocks);
        let snapshot = source.export(ExportMode::snapshot()).unwrap();
        let warm = load(&snapshot);
        black_box(warm.export(ExportMode::all_updates()).unwrap());
        let aligned = IdSpan::new(PEER, blocks[1].start, blocks[2].end);
        let mixed = IdSpan::new(PEER, blocks[0].start + 1, blocks[3].end - 1);
        for (name, span) in [
            ("aligned_two", aligned),
            ("mixed_two_edges_two_whole", mixed),
        ] {
            let default_bytes = export(&warm, span, false);
            let reuse_bytes = export(&warm, span, true);
            assert_eq!(
                serde_json::to_value(decode_update_json(&default_bytes).unwrap()).unwrap(),
                serde_json::to_value(decode_update_json(&reuse_bytes).unwrap()).unwrap()
            );
            for reuse in [false, true] {
                for cold in [true, false] {
                    let diagnostic = load(&snapshot);
                    if !cold {
                        black_box(diagnostic.export(ExportMode::all_updates()).unwrap());
                    }
                    let cache_facts: Vec<_> = diagnostic.update_source_blocks(&[span]).into_iter().map(|b|json!({"start":b.start,"end":b.end,"cache":format!("{:?}",b.cache)})).collect();
                    let (bytes, work) = observed(|| export(&diagnostic, span, reuse));
                    let latency = if cold {
                        // Source restoration is excluded; exported source starts Bytes-only.
                        // Borrow input to keep LoroDoc destruction outside the timer.
                        let mut samples = Vec::with_capacity(n);
                        for i in 0..n + 5 {
                            let doc = load(&snapshot);
                            let start = Instant::now();
                            let output = export(&doc, span, reuse);
                            let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
                            if i >= 5 {
                                samples.push(elapsed);
                            }
                            black_box(output);
                        }
                        distribution(samples)
                    } else {
                        measure(n, || (), |()| export(&warm, span, reuse))
                    };
                    rows.push(json!({"kind":"E_export","source_blocks":blocks.len(),"source_atomic_ops":next,"shape":name,"span":[span.counter.start,span.counter.end],"E":reuse,"source_cache":if cold {"cold_snapshot_source"} else {"warm_both"},"selected_source_cache":cache_facts,"output_bytes":bytes.len(),"packing_equal":default_bytes==reuse_bytes,"work":work,"latency":latency}));
                }
            }
        }
        // Keep exact same sparse native span while unrelated history in this SAME
        // ordinary Update grows. E chooses stored bytes before R timing starts.
        let query = IdSpan::new(PEER, 700, 716);
        for e in [false, true] {
            let bytes = if e {
                warm.export_updates_reusing_blocks(&VersionVector::default())
                    .unwrap()
            } else {
                warm.export(ExportMode::all_updates()).unwrap()
            };
            let selected = full_select(&bytes, query);
            assert_eq!(selected.len(), 16);
            let expected = serde_json::to_value(selected).unwrap();
            let reader = StoredUpdateReader::new(bytes.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(reader.query_json(query).unwrap()).unwrap(),
                expected
            );
            let (index, index_work) = observed(|| StoredUpdateReader::new(bytes.clone()).unwrap());
            let (_, full_work) = observed(|| full_select(&bytes, query));
            let (_, cold_work) = observed(|| {
                StoredUpdateReader::new(bytes.clone())
                    .unwrap()
                    .query_json(query)
                    .unwrap()
            });
            let (_, warm_work) = observed(|| index.query_json(query).unwrap());
            let full_latency = measure(
                n,
                || bytes.clone(),
                |input| {
                    let out = full_select(&input, query);
                    (input, out)
                },
            );
            // Reader (and owned input) destruction must not contaminate query timing.
            let cold_latency = measure(
                n,
                || bytes.clone(),
                |input| {
                    let reader = StoredUpdateReader::new(input).unwrap();
                    let out = reader.query_json(query).unwrap();
                    (reader, out)
                },
            );
            let warm_latency = measure(n, || (), |()| reader.query_json(query).unwrap());
            for (mode, r, work, latency) in [
                ("full_decode_and_select", false, full_work, full_latency),
                ("cold_index_and_query", true, cold_work, cold_latency),
                ("warm_retained_reader", true, warm_work, warm_latency),
            ] {
                rows.push(json!({"kind":"R_history","source_blocks":blocks.len(),"source_atomic_ops":next,"stored_update_blocks":reader.blocks().len(),"query_span":[700,716],"selected_atomic_ops":16,"E":e,"R":r,"read_mode":mode,"input_bytes":bytes.len(),"index_work":index_work,"work":work,"latency":latency}));
            }
        }
        eprintln!("completed {target_blocks} source blocks, {next} native operations");
    }
    let git = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    println!("{}",serde_json::to_string_pretty(&json!({"benchmark":"codec_only_update_reuse","baseline":"531a959f5322f9fd42f30b5de65f1047a721b498","local_head":git,"provider_checkpoint":"d50c7c08fc37de6a07c38dfa79c1f56b85759669","provider_note":"runtime source matches published provider checkpoint; local HEAD can differ when index alignment is unavailable","samples_per_cell":n,"warmups":5,"clock":"Instant wall time","counter_timing":"disabled; work observed separately","exclusions":"source snapshot load, owned input Vec preparation, output destruction; no SQLite, application edit, live replay or merge timing","header_input_bytes":"payload sizes passed to header parser, not physical bytes read","rows":rows})).unwrap());
}
