//! Fresh-process requested-heap and RSS observations; NOT a latency benchmark.
use loro::{
    decode_update_json, CommitOptions, ExportMode, IdSpan, JsonChange, LoroDoc, StoredUpdateReader,
};
use serde_json::{json, Value};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    path::Path,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
};
struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ZEROED: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
fn added(n: usize, kind: usize) {
    let live = LIVE.fetch_add(n, Relaxed) + n;
    if ACTIVE.load(Relaxed) {
        PEAK.fetch_max(live, Relaxed);
        REQUESTED.fetch_add(n, Relaxed);
        if kind == 0 {
            ALLOCS.fetch_add(1, Relaxed);
        } else {
            ZEROED.fetch_add(1, Relaxed);
        }
    }
}
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            added(l.size(), 0);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            added(l.size(), 1);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size(), Relaxed);
        if ACTIVE.load(Relaxed) {
            FREES.fetch_add(1, Relaxed);
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = System.realloc(p, l, n);
        if !q.is_null() {
            let live = if n >= l.size() {
                LIVE.fetch_add(n - l.size(), Relaxed) + n - l.size()
            } else {
                LIVE.fetch_sub(l.size() - n, Relaxed) - (l.size() - n)
            };
            if ACTIVE.load(Relaxed) {
                PEAK.fetch_max(live, Relaxed);
                REALLOCS.fetch_add(1, Relaxed);
                REQUESTED.fetch_add(n, Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Meter = Meter;
#[derive(Debug)]
struct Stats {
    base: usize,
    peak: usize,
    end: usize,
    alloc: usize,
    zeroed: usize,
    realloc: usize,
    free: usize,
    requested: usize,
}
fn begin() -> usize {
    ALLOCS.store(0, Relaxed);
    ZEROED.store(0, Relaxed);
    REALLOCS.store(0, Relaxed);
    FREES.store(0, Relaxed);
    REQUESTED.store(0, Relaxed);
    let b = LIVE.load(Relaxed);
    PEAK.store(b, Relaxed);
    ACTIVE.store(true, Relaxed);
    b
}
fn end(base: usize) -> Stats {
    ACTIVE.store(false, Relaxed);
    Stats {
        base,
        peak: PEAK.load(Relaxed),
        end: LIVE.load(Relaxed),
        alloc: ALLOCS.load(Relaxed),
        zeroed: ZEROED.load(Relaxed),
        realloc: REALLOCS.load(Relaxed),
        free: FREES.load(Relaxed),
        requested: REQUESTED.load(Relaxed),
    }
}
fn rss() -> Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let mut v = serde_json::Map::new();
    for line in status.lines() {
        if let Some((k, rest)) = line.split_once(':') {
            if ["VmRSS", "VmHWM"].contains(&k) {
                let kb: usize = rest.split_whitespace().next().unwrap().parse().unwrap();
                v.insert(format!("{k}_bytes"), json!(kb * 1024));
            }
        }
    }
    Value::Object(v)
}
fn select(bytes: &[u8]) -> Vec<JsonChange> {
    decode_update_json(bytes)
        .unwrap()
        .into_iter()
        .filter(|c| {
            assert_eq!(c.op_len(), 1);
            c.id.peer == 77 && (700..716).contains(&c.id.counter)
        })
        .collect()
}
fn prep(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let d = LoroDoc::new();
    d.set_peer_id(77).unwrap();
    d.set_change_merge_interval(0);
    let m = d.get_map("history");
    let mut next = 0;
    loop {
        let blocks = d.update_source_blocks(&[IdSpan::new(77, 0, i32::MAX)]);
        if blocks.len() >= 80 {
            assert_eq!(blocks.len(), 80);
            break;
        }
        for _ in 0..64 {
            let i = next;
            let text: String = (0..192)
                .map(|j| {
                    char::from_u32(0x20 + ((i * 7919 + j * 101 + (i ^ j) * 37) % 90) as u32)
                        .unwrap()
                })
                .collect();
            m.insert(&format!("key-{i}"), text).unwrap();
            d.commit_with(
                CommitOptions::new()
                    .timestamp(i as i64)
                    .commit_msg(&format!("bench-{i}")),
            );
            next += 1;
        }
    }
    let bytes = d.export(ExportMode::all_updates()).unwrap();
    let reader = StoredUpdateReader::new(bytes.clone()).unwrap();
    assert_eq!(reader.blocks().len(), 80);
    let full = select(&bytes);
    assert_eq!(full.len(), 16);
    let expected = serde_json::to_vec(&full).unwrap();
    assert_eq!(
        expected,
        serde_json::to_vec(&reader.query_json(IdSpan::new(77, 700, 716)).unwrap()).unwrap()
    );
    std::fs::write(dir.join("update.loro"), &bytes).unwrap();
    std::fs::write(dir.join("expected.json"), &expected).unwrap();
    std::fs::write(dir.join("fixture.json"),serde_json::to_vec_pretty(&json!({"peer":77,"blocks":80,"native_ops":next,"input_bytes":bytes.len(),"query_span":[700,716],"selected_ops":16,"update_xxh32":xxhash_rust::xxh32::xxh32(&bytes,0),"expected_xxh32":xxhash_rust::xxh32::xxh32(&expected,0)})).unwrap()).unwrap();
    println!("prepared80blocks,{next}ops,{}bytes", bytes.len());
}
fn run(mode: &str, dir: &Path) {
    assert!(matches!(
        mode,
        "full" | "full_compact" | "r_cold" | "r_warm"
    ));
    let bytes = std::fs::read(dir.join("update.loro")).unwrap();
    let input_bytes = bytes.len();
    let expected = std::fs::read(dir.join("expected.json")).unwrap();
    // Warm reader construction is explicitly outside the allocation scope, but
    // included in process lifetime RSS and scope baseline live requested bytes.
    let (input, warm) = if mode == "r_warm" {
        (None, Some(StoredUpdateReader::new(bytes).unwrap()))
    } else {
        (Some(bytes), None)
    };
    let before = rss();
    let base = begin();
    let (kept_input, reader, result) = match mode {
        "full" | "full_compact" => {
            let input = input.unwrap();
            let mut out = select(&input);
            if mode == "full_compact" {
                out.shrink_to_fit();
            }
            (Some(input), None, out)
        }
        "r_cold" => {
            let reader = StoredUpdateReader::new(input.unwrap()).unwrap();
            let out = reader.query_json(IdSpan::new(77, 700, 716)).unwrap();
            (None, Some(reader), out)
        }
        "r_warm" => {
            let reader = warm.unwrap();
            let out = reader.query_json(IdSpan::new(77, 700, 716)).unwrap();
            (None, Some(reader), out)
        }
        _ => unreachable!(),
    };
    assert_eq!(result.len(), 16);
    let canonical = serde_json::to_vec(&result).unwrap();
    assert_eq!(canonical, expected);
    let stats = end(base);
    let after = rss();
    // Hold equal selected output + canonical consumer bytes AND original input
    // (directly or owned by reader) until after all memory samples are captured.
    std::hint::black_box((&kept_input, &reader, &result, &canonical));
    let digest = xxhash_rust::xxh32::xxh32(&canonical, 0);
    println!(
        "{}",
        json!({"mode":mode,"pid":std::process::id(),"input_bytes":input_bytes,"selected_ops":result.len(),"selected_vec_capacity":result.capacity(),"canonical_result_bytes":canonical.len(),"result_xxh32":digest,"rss_before":before,"rss_with_result_retained":after,"heap":{"baseline_requested_live_bytes":stats.base,"peak_requested_live_bytes":stats.peak,"peak_requested_growth_bytes":stats.peak.saturating_sub(stats.base),"retained_requested_live_bytes":stats.end,"retained_requested_delta_bytes":stats.end as i128-stats.base as i128,"successful_alloc_calls":stats.alloc,"successful_alloc_zeroed_calls":stats.zeroed,"successful_realloc_calls":stats.realloc,"dealloc_calls":stats.free,"cumulative_requested_alloc_and_realloc_bytes":stats.requested},"scope":"input+expected preloaded; includes decode/index as mode specifies, selected JSON construction and canonical result consumption; output/input retained","rss_scope":"whole fresh process lifetime including startup/file loading and warm-reader setup; excludes separately executed fixture construction; VmHWM is not an isolated operation peak","allocator_scope":"Rust System requested sizes, not malloc usable bytes, fragmentation, mmap or RSS; realloc bytes count full successful new request, not physical copies or transient old+new coexistence inside System.realloc; no latency conclusions"})
    );
}
fn main() {
    let a: Vec<_> = std::env::args().collect();
    assert_eq!(
        a.len(),
        3,
        "usage: update_reuse_memory prep|full|full_compact|r_cold|r_warm FIXTURE_DIR"
    );
    let dir = Path::new(&a[2]);
    if a[1] == "prep" {
        prep(dir)
    } else {
        run(&a[1], dir)
    }
}
