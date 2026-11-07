//! Read-only state projection with original text operands kept out of values.

use super::{deleted_root_container_value_is_cleared, visible_container_value_is_empty, DocState};
use crate::{container::{idx::ContainerIdx, richtext::{AnchorType, richtext_state::RichtextStateChunk}},
    text_slice::TextSlice};
use loro_common::{ContainerID, ContainerType, IdFull, InternalString, LoroError, LoroResult, LoroValue};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::Ordering;

#[derive(Debug, Clone)]
pub struct NativeStateTextRun {
    pub start: IdFull,
    pub unicode_len: usize,
    pub slice: TextSlice,
}

/// A style anchor's logical Unicode boundary, independent of text chunking.
/// `id` names the start operation for both anchors of that style.
#[derive(Debug, Clone)]
pub struct NativeStateStyleAnchor {
    pub unicode_pos: usize,
    pub start: bool,
    pub id: IdFull,
    pub key: InternalString,
    pub value: LoroValue,
    pub info: u8,
}

#[derive(Debug, Clone)]
pub struct NativeStateText {
    pub container: ContainerID,
    pub runs: Vec<NativeStateTextRun>,
    pub anchors: Vec<NativeStateStyleAnchor>,
}

/// Same visible projection as get_deep_value, except Text values are container
/// slots whose immutable original operands are in `text`. This is read evidence,
/// not an import format or independent authority to accept a document.
#[derive(Debug, Clone)]
pub struct NativeStateProjection {
    pub value: LoroValue,
    pub text: Vec<NativeStateText>,
    // A raw nested LoroValue may itself contain a Container value. Only native
    // Text edges explicitly visited by the projection are substitution slots.
    text_paths: FxHashMap<Vec<StatePath>, ContainerID>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum StatePath { Key(String), Index(usize) }

fn invalid() -> LoroError { LoroError::DecodeError("invalid reference state projection".into()) }

impl NativeStateProjection {
    /// Preserve plain deep-value equality. Native IDs and style anchors remain
    /// available to callers, but are not additional acceptance conditions here.
    /// Physical text run boundaries need not agree.
    pub fn same_value(&self, other: &Self) -> LoroResult<bool> {
        let left: FxHashMap<_, _> = self.text.iter().map(|text| (text.container.clone(), text)).collect();
        let right: FxHashMap<_, _> = other.text.iter().map(|text| (text.container.clone(), text)).collect();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compare_values(&self.value, &other.value, &left, &right,
                &self.text_paths, &other.text_paths, &mut Vec::new())
        })).unwrap_or_else(|_| Err(invalid()))
    }
}

fn compare_values(
    a: &LoroValue, b: &LoroValue,
    left: &FxHashMap<ContainerID, &NativeStateText>,
    right: &FxHashMap<ContainerID, &NativeStateText>,
    left_paths: &FxHashMap<Vec<StatePath>, ContainerID>,
    right_paths: &FxHashMap<Vec<StatePath>, ContainerID>,
    path: &mut Vec<StatePath>,
) -> LoroResult<bool> {
    let at = left_paths.get(path).and_then(|id| left.get(id)).copied();
    let bt = right_paths.get(path).and_then(|id| right.get(id)).copied();
    match (at, bt) {
        (Some(a), Some(b)) => return compare_text(a, b),
        (Some(a), None) => return b.as_string().map_or(Ok(false), |b| text_matches_bytes(a, b.as_bytes())),
        (None, Some(b)) => return a.as_string().map_or(Ok(false), |a| text_matches_bytes(b, a.as_bytes())),
        (None, None) => {}
    }
    match (a, b) {
        (LoroValue::List(a), LoroValue::List(b)) => {
            if a.len() != b.len() { return Ok(false); }
            for (index, (a, b)) in a.iter().zip(b.iter()).enumerate() {
                path.push(StatePath::Index(index));
                let equal = compare_values(a, b, left, right, left_paths, right_paths, path)?;
                path.pop();
                if !equal { return Ok(false); }
            }
            Ok(true)
        }
        (LoroValue::Map(a), LoroValue::Map(b)) => {
            if a.len() != b.len() { return Ok(false); }
            for (key, a) in a.iter() {
                let Some(b) = b.get(key) else { return Ok(false); };
                path.push(StatePath::Key(key.clone()));
                let equal = compare_values(a, b, left, right, left_paths, right_paths, path)?;
                path.pop();
                if !equal { return Ok(false); }
            }
            Ok(true)
        }
        _ => Ok(a == b),
    }
}

fn text_matches_bytes(text: &NativeStateText, expected: &[u8]) -> LoroResult<bool> {
    let mut offset = 0usize;
    let mut equal = true;
    for run in &text.runs {
        run.slice.try_for_each_chunk("state.reference_compare", |bytes| {
            let end = offset.checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
            equal &= expected.get(offset..end) == Some(bytes);
            offset = end;
            Ok(())
        }).map_err(|_| invalid())?;
    }
    Ok(equal && offset == expected.len())
}

fn compare_text(a: &NativeStateText, b: &NativeStateText) -> LoroResult<bool> {
    let (mut ai, mut bi, mut ap, mut bp) = (0, 0, 0, 0);
    loop {
        while ai < a.runs.len() && ap == a.runs[ai].slice.len() { ai += 1; ap = 0; }
        while bi < b.runs.len() && bp == b.runs[bi].slice.len() { bi += 1; bp = 0; }
        match (a.runs.get(ai), b.runs.get(bi)) {
            (None, None) => return Ok(true),
            (Some(a), Some(b)) => {
                let len = (a.slice.len() - ap).min(b.slice.len() - bp);
                if !a.slice.slice_clone(ap..ap + len).try_bytes_equal(&b.slice.slice_clone(bp..bp + len))
                    .map_err(|_| invalid())? { return Ok(false); }
                ap += len; bp += len;
            }
            _ => return Ok(false),
        }
    }
}

impl DocState {
    pub(crate) fn get_deep_value_with_text_slices(&mut self) -> LoroResult<NativeStateProjection> {
        // Match preferred_root_containers without its plaintext value read.
        let roots = self.arena.top_level_root_containers(self.store.load_root_containers());
        let mut selected = FxHashMap::default();
        for idx in roots {
            let Some(cid) = self.arena.idx_to_id(idx) else { continue; };
            if !self.store.contains_id(&cid) { continue; }
            let Some(name) = self.root_container_name(idx) else { continue; };
            let empty = self.reference_root_is_empty(idx)?;
            match selected.entry(name) {
                std::collections::hash_map::Entry::Vacant(entry) => { entry.insert((idx, empty)); }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().1 || !empty { entry.insert((idx, empty)); }
                }
            }
        }
        let hidden = self.config.hide_empty_root_containers.load(Ordering::Relaxed);
        let deleted = self.config.deleted_root_containers.lock().clone();
        let mut value = FxHashMap::default();
        let mut text = FxHashMap::default();
        let mut active = FxHashSet::default();
        let mut text_paths = FxHashMap::default();
        for (name, (idx, empty)) in selected {
            if hidden && empty { continue; }
            let cid = self.arena.idx_to_id(idx).ok_or_else(invalid)?;
            if deleted.contains(&cid) && idx.get_type() == ContainerType::Text && empty { continue; }
            let mut path = vec![StatePath::Key(name.to_string())];
            let nested = self.reference_container_value(idx, &mut text, &mut active, &mut text_paths, &mut path)?;
            if deleted.contains(&cid) && idx.get_type() != ContainerType::Text
                && deleted_root_container_value_is_cleared(idx.get_type(), &nested) { continue; }
            value.insert(name.to_string(), nested);
        }
        let mut text: Vec<_> = text.into_values().collect();
        text.sort_by_key(|text| text.container.to_bytes());
        Ok(NativeStateProjection { value: LoroValue::Map(value.into()), text, text_paths })
    }

    fn reference_root_is_empty(&mut self, idx: ContainerIdx) -> LoroResult<bool> {
        if idx.get_type() == ContainerType::Text {
            return Ok(self.store.resident_reference_text(idx)?.is_none_or(|text| text.get_text_id_runs().is_empty()));
        }
        let value = self.store.try_get_value_ephemeral(idx)?.unwrap_or_else(|| idx.get_type().default_value());
        Ok(visible_container_value_is_empty(idx.get_type(), &value))
    }

    fn reference_container_value(
        &mut self, idx: ContainerIdx, texts: &mut FxHashMap<ContainerID, NativeStateText>,
        active: &mut FxHashSet<ContainerID>,
        text_paths: &mut FxHashMap<Vec<StatePath>, ContainerID>, path: &mut Vec<StatePath>,
    ) -> LoroResult<LoroValue> {
        let cid = self.arena.idx_to_id(idx).ok_or_else(invalid)?;
        if idx.get_type() == ContainerType::Text {
            if !texts.contains_key(&cid) {
                let mut result = NativeStateText { container: cid.clone(), runs: Vec::new(), anchors: Vec::new() };
                let mut unicode_pos = Some(0usize);
                if let Some(text) = self.store.resident_reference_text(idx)? {
                    text.iter_raw(&mut |chunk| match chunk {
                        RichtextStateChunk::Text(text) => {
                            let len = text.unicode_len() as usize;
                            result.runs.push(NativeStateTextRun { start: text.id_full(), unicode_len: len,
                                slice: text.bytes().clone() });
                            unicode_pos = unicode_pos.and_then(|pos| pos.checked_add(len));
                        }
                        RichtextStateChunk::Style { style, anchor_type } => {
                            if let Some(pos) = unicode_pos {
                                result.anchors.push(NativeStateStyleAnchor { unicode_pos: pos,
                                    start: *anchor_type == AnchorType::Start, id: style.id_full(),
                                    key: style.key.clone(), value: style.value.clone(), info: style.info.to_byte() });
                            }
                        }
                    });
                }
                unicode_pos.ok_or_else(invalid)?;
                texts.insert(cid.clone(), result);
            }
            text_paths.insert(path.clone(), cid.clone());
            return Ok(LoroValue::Container(cid));
        }
        if !active.insert(cid.clone()) { return Err(invalid()); }
        let value = self.store.try_get_value_ephemeral(idx)?.unwrap_or_else(|| idx.get_type().default_value());
        let result = match value {
            LoroValue::List(mut list) => {
                if idx.get_type() == ContainerType::Tree {
                    self.reference_tree_metadata(list.make_mut(), texts, active, text_paths, path)?;
                } else {
                    for (index, item) in list.make_mut().iter_mut().enumerate() {
                        if let LoroValue::Container(cid) = item {
                            let child = self.arena.register_container(cid);
                            path.push(StatePath::Index(index));
                            *item = self.reference_container_value(child, texts, active, text_paths, path)?;
                            path.pop();
                        }
                    }
                }
                LoroValue::List(list)
            }
            LoroValue::Map(mut map) => {
                let children = self.mergeable_children_from_value(&cid, &map);
                for (key, item) in map.make_mut().iter_mut() {
                    if let LoroValue::Container(cid) = item {
                        let child = self.arena.register_container(cid);
                        path.push(StatePath::Key(key.clone()));
                        *item = self.reference_container_value(child, texts, active, text_paths, path)?;
                        path.pop();
                    }
                }
                for (key, cid) in children {
                    let child = self.arena.register_container(&cid);
                    path.push(StatePath::Key(key.to_string()));
                    let value = self.reference_container_value(child, texts, active, text_paths, path)?;
                    path.pop();
                    map.make_mut().insert(key.to_string(), value);
                }
                LoroValue::Map(map)
            }
            LoroValue::Container(_) => return Err(invalid()),
            value => value,
        };
        active.remove(&cid);
        Ok(result)
    }

    fn reference_tree_metadata(
        &mut self, nodes: &mut [LoroValue], texts: &mut FxHashMap<ContainerID, NativeStateText>,
        active: &mut FxHashSet<ContainerID>,
        text_paths: &mut FxHashMap<Vec<StatePath>, ContainerID>, path: &mut Vec<StatePath>,
    ) -> LoroResult<()> {
        for (index, node) in nodes.iter_mut().enumerate() {
            path.push(StatePath::Index(index));
            let map = node.as_map_mut().ok_or_else(invalid)?.make_mut();
            let meta = map.get_mut("meta").ok_or_else(invalid)?;
            let cid = meta.as_container().ok_or_else(invalid)?;
            let child = self.arena.register_container(cid);
            path.push(StatePath::Key("meta".into()));
            *meta = self.reference_container_value(child, texts, active, text_paths, path)?;
            path.pop();
            let children = map.get_mut("children").and_then(LoroValue::as_list_mut).ok_or_else(invalid)?;
            path.push(StatePath::Key("children".into()));
            self.reference_tree_metadata(children.make_mut(), texts, active, text_paths, path)?;
            path.pop();
            path.pop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cursor::PosType, LoroDoc};

    fn projection(doc: &LoroDoc) -> NativeStateProjection {
        doc.app_state().lock().get_deep_value_with_text_slices().unwrap()
    }

    fn expand(value: &LoroValue, text: &[NativeStateText]) -> LoroValue {
        match value {
            LoroValue::Container(cid) => {
                let text = text.iter().find(|text| &text.container == cid).unwrap();
                let mut bytes = Vec::new();
                for run in &text.runs {
                    run.slice.try_for_each_chunk("test.reference_state", |part| {
                        bytes.extend_from_slice(part); Ok(())
                    }).unwrap();
                }
                String::from_utf8(bytes).unwrap().into()
            }
            LoroValue::Map(map) => LoroValue::Map(map.iter().map(|(key, value)|
                (key.clone(), expand(value, text))).collect::<loro_common::LoroMapValue>()),
            LoroValue::List(list) => LoroValue::List(list.iter().map(|value|
                expand(value, text)).collect::<Vec<_>>().into()),
            value => value.clone(),
        }
    }

    #[test]
    fn reference_state_projection_preserves_styles_and_hidden_empty_roots() {
        let doc = LoroDoc::new_auto_commit();
        let text = doc.get_text("body");
        text.insert(0, "a玉🙂", PosType::Unicode).unwrap();
        text.mark(1, 3, "bold", true.into(), PosType::Unicode).unwrap();
        doc.get_text("empty");
        doc.get_map("metadata").insert("answer", 42).unwrap();
        doc.commit_then_renew();
        doc.config.set_hide_empty_root_containers(true);
        let expected = doc.get_deep_value();
        crate::text_slice::reset_text_copy_metrics();
        let actual = projection(&doc);
        assert!(crate::text_slice::text_copy_metrics().values()
            .all(|metric| metric.native_bytes == 0 && metric.extent_bytes == 0));
        assert_eq!(expand(&actual.value, &actual.text), expected);
        assert_eq!(actual.text.len(), 1);
        assert_eq!(actual.text[0].anchors.len(), 2);
        assert_eq!(actual.text[0].anchors[0].unicode_pos, 1);
        assert_eq!(actual.text[0].anchors[1].unicode_pos, 3);
    }

    #[test]
    fn reference_state_comparison_ignores_physical_chunk_boundaries() {
        let doc = LoroDoc::new_auto_commit();
        doc.get_text("body").insert(0, "a玉🙂", PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let original = projection(&doc);
        let mut split = original.clone();
        let run = split.text[0].runs.remove(0);
        split.text[0].runs.push(NativeStateTextRun {
            start: run.start, unicode_len: 1, slice: run.slice.slice_clone(..1),
        });
        split.text[0].runs.push(NativeStateTextRun {
            start: loro_common::IdFull::new(run.start.peer, run.start.counter + 1, run.start.lamport + 1),
            unicode_len: 2, slice: run.slice.slice_clone(1..),
        });
        assert!(original.same_value(&split).unwrap());
        split.text[0].runs[0].slice = crate::text_slice::TextSlice::from_bytes(b"b");
        assert!(!original.same_value(&split).unwrap());
    }

    #[test]
    fn reference_state_projection_preserves_nested_mergeable_and_tree_edges() {
        use crate::{HandlerTrait, TreeParentId};

        let doc = LoroDoc::new_auto_commit();
        let nested = doc.get_map("root").ensure_mergeable_map("profile").unwrap();
        nested.ensure_mergeable_text("body").unwrap()
            .insert(0, "玉", PosType::Unicode).unwrap();
        // A marker can expose an empty child with no child state or history.
        nested.ensure_mergeable_text("empty").unwrap();
        let losing = nested.ensure_mergeable_text("kind").unwrap();
        losing.insert(0, "hidden", PosType::Unicode).unwrap();
        nested.ensure_mergeable_map("kind").unwrap().insert("visible", true).unwrap();

        let list = doc.get_list("list");
        let child = list.insert_container(0, crate::handler::TextHandler::new_detached()).unwrap();
        child.insert(0, "list", PosType::Unicode).unwrap();
        let tree = doc.get_tree("tree");
        let parent = tree.create(TreeParentId::Root).unwrap();
        let child = tree.create(TreeParentId::Node(parent)).unwrap();
        tree.get_meta(parent).unwrap().ensure_mergeable_text("label").unwrap()
            .insert(0, "parent", PosType::Unicode).unwrap();
        tree.get_meta(child).unwrap().ensure_mergeable_text("label").unwrap()
            .insert(0, "child", PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let expected = doc.get_deep_value();
        let actual = projection(&doc);
        assert_eq!(expand(&actual.value, &actual.text), expected);
        assert!(actual.text.iter().all(|text| text.container != losing.id()));
        assert!(actual.same_value(&actual).unwrap());
    }

    #[test]
    fn reference_state_slots_do_not_reinterpret_opaque_nested_container_values() {
        let doc = LoroDoc::new_auto_commit();
        doc.get_text("body").insert(0, "a", PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let mut original = projection(&doc);
        let cid = original.text[0].container.clone();
        original.value.as_map_mut().unwrap().make_mut().insert("opaque".into(),
            LoroValue::List(vec![LoroValue::Container(cid)].into()));
        let mut changed = original.clone();
        changed.value.as_map_mut().unwrap().make_mut().insert("opaque".into(),
            LoroValue::List(vec![LoroValue::String("a".into())].into()));
        assert!(!original.same_value(&changed).unwrap());
    }

    #[test]
    fn reference_state_comparison_reports_late_resolver_failure() {
        struct Fails(bool);
        impl crate::text_slice::ExtentResolver for Fails {
            fn with_bytes(&self, _: u64, _: std::ops::Range<usize>, _: &'static str,
                _: &mut dyn FnMut(&[u8])) -> std::io::Result<()> {
                assert!(!self.0, "late resolver panic");
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            }
        }
        let doc = LoroDoc::new_auto_commit();
        doc.get_text("body").insert(0, "a", PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let original = projection(&doc);
        for panic in [false, true] {
            let mut broken = original.clone();
            broken.text[0].runs[0].slice = TextSlice::extent(std::sync::Arc::new(Fails(panic)), 1, 0..1);
            assert!(original.same_value(&broken).is_err());
        }
    }

    #[test]
    fn reference_state_comparison_detects_state_history_divergence() {
        let source = LoroDoc::new_auto_commit();
        source.set_peer_id(93).unwrap();
        source.get_text("body").insert(0, "abc", PosType::Unicode).unwrap();
        source.commit_then_renew();
        let donor = LoroDoc::new_auto_commit();
        donor.set_peer_id(93).unwrap();
        donor.get_text("body").insert(0, "xyz", PosType::Unicode).unwrap();
        donor.commit_then_renew();
        let mut snapshot = source.with_barrier(|| crate::encoding::fast_snapshot::encode_snapshot_inner(&source)).unwrap();
        let state = donor.with_barrier(|| crate::encoding::fast_snapshot::encode_snapshot_inner(&donor)).unwrap();
        snapshot.state_bytes = state.state_bytes;
        let divergent = LoroDoc::new();
        crate::encoding::fast_snapshot::decode_snapshot_inner(snapshot, &divergent, Default::default()).unwrap();
        // Materialize ordinary state only in this adversarial fixture. Its
        // independently decoded history is deliberately identical to source.
        assert_eq!(divergent.get_text("body").to_string(), "xyz");
        divergent.get_text("body").get_text_id_runs();
        assert_eq!(source.oplog_vv(), divergent.oplog_vv());
        assert!(!projection(&source).same_value(&projection(&divergent)).unwrap());
    }
}
