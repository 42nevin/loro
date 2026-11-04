//! Opt-in thread-local work accounting. Leave disabled in latency measurements.
use std::cell::Cell;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpdateReuseWork {
    pub selected_blocks: u64,
    pub sliced_changes: u64,
    pub encoded_blocks: u64,
    pub encoded_changes: u64,
    pub encoded_ops: u64,
    pub encoded_atom_ops: u64,
    pub encoded_bytes: u64,
    pub decoded_blocks: u64,
    pub decoded_changes: u64,
    pub decoded_ops: u64,
    pub decoded_atom_ops: u64,
    pub decoded_bytes: u64,
    pub copied_bytes: u64,
    pub eligible_whole_blocks: u64,
    pub reused_cached_blocks: u64,
    pub newly_encoded_whole_blocks: u64,
    pub checksum_scanned_bytes: u64,
    pub index_builds: u64,
    pub indexed_blocks: u64,
    /// Payload bytes presented to header parsing, NOT actual bytes read by the parser.
    pub header_input_bytes: u64,
    pub query_metadata_scans: u64,
}
thread_local! {
    static WORK: Cell<Option<UpdateReuseWork>> = const { Cell::new(None) };
}
/// Start a fresh measured region on this thread. Not nestable.
pub fn start() {
    WORK.set(Some(UpdateReuseWork::default()));
}
/// Read and disable accounting on this thread.
pub fn finish() -> UpdateReuseWork {
    WORK.replace(None).unwrap_or_default()
}
pub(crate) fn record(f: impl FnOnce(&mut UpdateReuseWork)) {
    WORK.with(|cell| {
        if let Some(mut w) = cell.get() {
            f(&mut w);
            cell.set(Some(w));
        }
    });
}
