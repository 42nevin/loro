//! Experimental reuse of the existing FastUpdates format. No remote import bypass.
use crate::{
    arena::SharedArena,
    change::{Change, ChangeRef},
    encoding::{parse_header_and_body, EncodeMode},
    op::RemoteOp,
    oplog::{change_store::block_encode, convert_change_to_remote},
};
use loro_common::{Counter, IdSpan, LoroError, LoroResult, PeerID};
use rle::{HasLength, Sliceable};
use std::collections::BTreeMap;

pub type NativeUpdateChange = Change<RemoteOp<'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateBlockCache {
    Changes,
    Bytes,
    Both,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSourceBlock {
    pub peer: PeerID,
    pub start: Counter,
    pub end: Counter,
    pub cache: UpdateBlockCache,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateBlockMeta {
    pub peer: PeerID,
    pub start: Counter,
    pub end: Counter,
    /// Absolute offset in the original ordinary Update, after its length prefix.
    pub payload_offset: usize,
    pub payload_len: usize,
}

/// Immutable index over an ordinary Update already accepted into trusted local storage.
/// Construction verifies the entire envelope checksum and every block's framing/header.
/// Only selected payloads are decoded on queries. This is NOT a replacement for import
/// validation of remote input: it does not establish dependency availability or apply ops.
#[derive(Debug)]
pub struct StoredUpdateReader {
    bytes: Vec<u8>,
    blocks: Vec<UpdateBlockMeta>,
    by_peer: BTreeMap<PeerID, Vec<usize>>,
}
impl StoredUpdateReader {
    pub fn new(bytes: Vec<u8>) -> LoroResult<Self> {
        let parsed = parse_header_and_body(&bytes, true)?;
        if parsed.mode != EncodeMode::FastUpdates {
            return Err(LoroError::DecodeError(
                "StoredUpdateReader requires ordinary Update bytes".into(),
            ));
        }
        crate::update_reuse_work::record(|w| w.index_builds += 1);
        let mut reader = parsed.body;
        let mut blocks = Vec::new();
        let mut by_peer: BTreeMap<PeerID, Vec<usize>> = BTreeMap::new();
        while !reader.is_empty() {
            let len = leb128::read::unsigned(&mut reader)
                .map_err(|_| LoroError::DecodeDataCorruptionError)?;
            let len = usize::try_from(len).map_err(|_| LoroError::DecodeDataCorruptionError)?;
            if len == 0 || len > reader.len() {
                return Err(LoroError::DecodeDataCorruptionError);
            }
            let offset = bytes.len() - reader.len();
            let payload = &reader[..len];
            crate::update_reuse_work::record(|w| {
                w.indexed_blocks += 1;
                w.header_input_bytes += len as u64;
            });
            let header = block_encode::decode_header(payload)?;
            let Some(&end) = header.counters.last() else {
                return Err(LoroError::DecodeDataCorruptionError);
            };
            if header.n_changes == 0
                || header.counter < 0
                || end <= header.counter
                || header.counters.len() != header.n_changes + 1
                || header.counters.first().copied() != Some(header.counter)
                || header.counters.windows(2).any(|pair| pair[0] >= pair[1])
            {
                return Err(LoroError::DecodeDataCorruptionError);
            }
            by_peer.entry(header.peer).or_default().push(blocks.len());
            blocks.push(UpdateBlockMeta {
                peer: header.peer,
                start: header.counter,
                end,
                payload_offset: offset,
                payload_len: len,
            });
            reader = &reader[len..];
        }
        // Stored Updates produced by exporters have unique native ranges. Reject
        // duplicated/overlapping payloads explicitly rather than silently return
        // duplicate operations; normal remote import keeps its existing behavior.
        for indices in by_peer.values_mut() {
            indices.sort_by_key(|&i| blocks[i].start);
            for pair in indices.windows(2) {
                if blocks[pair[0]].end > blocks[pair[1]].start {
                    return Err(LoroError::DecodeError(
                        "StoredUpdateReader rejects overlapping block ranges".into(),
                    ));
                }
            }
        }
        Ok(Self {
            bytes,
            blocks,
            by_peer,
        })
    }
    pub fn blocks(&self) -> &[UpdateBlockMeta] {
        &self.blocks
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn selected(&self, mut span: IdSpan) -> LoroResult<(SharedArena, Vec<Change>)> {
        span.normalize_();
        let arena = SharedArena::new();
        let mut answer = Vec::new();
        span.counter.start = span.counter.start.max(0);
        span.counter.end = span.counter.end.max(0);
        if span.counter.start >= span.counter.end {
            return Ok((arena, answer));
        }
        if let Some(indices) = self.by_peer.get(&span.peer) {
            for &i in indices {
                crate::update_reuse_work::record(|w| w.query_metadata_scans += 1);
                let b = &self.blocks[i];
                if span.counter.start >= b.end || span.counter.end <= b.start {
                    continue;
                }
                crate::update_reuse_work::record(|w| w.selected_blocks += 1);
                let changes = block_encode::decode_block(
                    &self.bytes[b.payload_offset..b.payload_offset + b.payload_len],
                    &arena,
                    None,
                )?;
                for c in changes {
                    let start = (span.counter.start - c.id.counter).max(0) as usize;
                    let end = ((span.counter.end - c.id.counter).max(0) as usize).min(c.atom_len());
                    if start == 0 && end == c.atom_len() {
                        answer.push(c);
                    } else if start < end {
                        answer.push(c.slice(start, end));
                    }
                }
            }
        }
        answer.sort_by_key(|c| (c.lamport(), c.id()));
        Ok((arena, answer))
    }
    /// Native owned changes, including IDs, deps, timestamps, messages and operation content.
    pub fn query(&self, span: IdSpan) -> LoroResult<Vec<NativeUpdateChange>> {
        let (arena, changes) = self.selected(span)?;
        Ok(changes
            .iter()
            .map(|c| convert_change_to_remote(&arena, c))
            .collect())
    }
    /// Convenience conversion for history consumers. Native decoding and slicing are
    /// identical to query; JSON construction is additional work, not decoding avoidance.
    pub fn query_json(&self, span: IdSpan) -> LoroResult<Vec<crate::json::JsonChange>> {
        let (arena, changes) = self.selected(span)?;
        Ok(changes
            .iter()
            .map(|c| {
                crate::encoding::json_schema::encode_change(ChangeRef::from_change(c), &arena, None)
            })
            .collect())
    }
}

pub(crate) fn normalize_spans(spans: &[IdSpan]) -> Vec<IdSpan> {
    let mut peers: BTreeMap<PeerID, Vec<(Counter, Counter)>> = BTreeMap::new();
    for mut span in spans.iter().copied() {
        span.normalize_();
        let start = span.counter.start.max(0);
        let end = span.counter.end.max(0);
        if start < end {
            peers.entry(span.peer).or_default().push((start, end));
        }
    }
    let mut result: Vec<IdSpan> = Vec::new();
    for (peer, mut ranges) in peers {
        ranges.sort_unstable();
        for (start, end) in ranges {
            if let Some(last) = result.last_mut() {
                if last.peer == peer && start <= last.counter.end {
                    last.counter.end = last.counter.end.max(end);
                    continue;
                }
            }
            result.push(IdSpan::new(peer, start, end));
        }
    }
    result
}

/// Decode every block with the existing ordinary Update decoder, then convert to JSON.
/// This comparison helper does not apply changes or drop changes with missing deps.
/// It is not a replacement for remote import validation.
pub fn decode_update_json(bytes: &[u8]) -> LoroResult<Vec<crate::json::JsonChange>> {
    let parsed = parse_header_and_body(bytes, true)?;
    if parsed.mode != EncodeMode::FastUpdates {
        return Err(LoroError::DecodeError(
            "decode_update_json requires ordinary Update bytes".into(),
        ));
    }
    let mut oplog = crate::OpLog::new(std::sync::Arc::new(crate::sync::AtomicUsize::new(0)));
    let changes = crate::encoding::fast_snapshot::decode_updates(
        &mut oplog,
        bytes::Bytes::copy_from_slice(parsed.body),
    )?;
    Ok(changes
        .iter()
        .map(|c| {
            crate::encoding::json_schema::encode_change(
                ChangeRef::from_change(c),
                &oplog.arena,
                None,
            )
        })
        .collect())
}
