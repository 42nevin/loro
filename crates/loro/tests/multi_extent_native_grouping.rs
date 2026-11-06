#![cfg(feature = "shared_arena_experiment")]
//! Real allocation calls, distinct immutable payload extents, real native RLE.
use loro::{ExportMode, ExtentResolver, LoroDoc, TextExtentAllocator, TextSlice, VersionVector, ID};
use loro::native_text_observation::{start_native_text_observations, take_native_text_observations, NativeTextObservation};
use std::{collections::BTreeMap, io, ops::Range, sync::{Arc, RwLock}};
#[derive(Default)]
struct Store(RwLock<BTreeMap<u64, Vec<u8>>>);
impl ExtentResolver for Store {
    fn with_bytes(&self, id: u64, range: Range<usize>, _: &'static str, visit: &mut dyn FnMut(&[u8])) -> io::Result<()> {
        let lock = self.0.read().unwrap();
        let bytes = lock.get(&id).and_then(|v| v.get(range)).ok_or(io::ErrorKind::NotFound)?;
        visit(bytes);
        Ok(())
    }
    fn record_materialization(&self, _: u64, _: Range<usize>, _: &'static str, _: usize) {
        assert!(self.0.try_write().is_ok(), "materialization notification must follow bounded view release");
    }
}
struct Allocator { store: Arc<Store>, next: u64 }
impl TextExtentAllocator for Allocator {
    fn allocate_text(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> io::Result<TextSlice> {
        let id = self.next; self.next += 1;
        let mut bytes = vec![0; len]; generate(&mut bytes);
        assert!(self.store.0.write().unwrap().insert(id, bytes).is_none());
        Ok(TextSlice::extent(self.store.clone(), id, 0..len))
    }
}
#[test]
fn distinct_allocations_group_and_native_cross_boundary_slice_preserves_original_ranges() {
    let doc = LoroDoc::new(); doc.set_peer_id(91).unwrap();
    let text = doc.get_text("body");
    let mut allocator = Allocator { store: Arc::new(Store::default()), next: 100 };
    start_native_text_observations(); loro::reset_text_copy_metrics();
    text.insert_unicode_with_allocator(0, "a玉", &mut allocator).unwrap();
    text.insert_unicode_with_allocator(2, "🙂Z", &mut allocator).unwrap();
    doc.commit();
    let events = take_native_text_observations();
    let final_ops = events.iter().find_map(|e| match e { NativeTextObservation::Finalize { native_op_count, text_ops, .. } => Some((native_op_count,text_ops)), _ => None }).unwrap();
    assert_eq!(*final_ops.0, 1, "distinct immutable extents must group in the actual native Op");
    assert_eq!(final_ops.1[0].op.scalar_count, 4);
    let ranges = &final_ops.1[0].original_ranges;
    assert_eq!(ranges.len(), 2);
    assert_eq!((ranges[0].extent, ranges[0].range.clone()), (Some(100), 0..4));
    assert_eq!((ranges[1].extent, ranges[1].range.clone()), (Some(101), 0..5));
    assert!(events.iter().any(|e| matches!(e, NativeTextObservation::Merge { result, .. } if result.scalar_count == 4)));
    let copies = loro::text_copy_metrics();
    assert_eq!(copies["allocated.input_string"].native_bytes, 9);
    assert_eq!(copies["allocated.input_string"].native_calls, 2);
    // Native partial exports force scalar cuts within both original extents.
    start_native_text_observations();
    let cut = doc.export(ExportMode::updates_in_range(vec![loro::IdSpan::new(91, 1, 3)])).unwrap();
    let slices = take_native_text_observations();
    let crossing = slices.iter().find_map(|e| match e {
        NativeTextObservation::Slice { result, .. } if result.counter == 1 && result.scalar_count == 2 => Some(result),
        _ => None,
    }).expect("actual cross-boundary native slice");
    assert_eq!(crossing.backing_parts.iter().map(|p|(p.extent,p.range.clone())).collect::<Vec<_>>(),
        vec![(Some(100),1..4),(Some(101),0..4)]);
    let receiver = LoroDoc::new();
    let mut prefix = VersionVector::default(); prefix.set_end(ID::new(91, 1));
    receiver.import(&doc.export(ExportMode::updates_till(&prefix)).unwrap()).unwrap();
    receiver.import(&cut).unwrap();
    assert_eq!(receiver.get_text("body").to_string(), "a玉🙂");
    receiver.import(&doc.export(ExportMode::all_updates()).unwrap()).unwrap();
    assert_eq!(receiver.get_text("body").to_string(), "a玉🙂Z");
    drop(allocator);
    assert_eq!(text.to_string(), "a玉🙂Z");
    let clone = LoroDoc::new(); clone.import(&doc.export(ExportMode::snapshot()).unwrap()).unwrap();
    assert_eq!(clone.get_text("body").to_string(), "a玉🙂Z");
}

#[test]
fn composite_identity_clipping_and_temporary_copies_are_explicit() {
    let store = Arc::new(Store::default());
    store.0.write().unwrap().extend([(1, b"ab".to_vec()), (2, "玉🙂".as_bytes().to_vec()), (3, b"xy".to_vec())]);
    let left = TextSlice::extent(store.clone(), 1, 0..2);
    let mut joined = left.clone();
    loro::reset_text_copy_metrics();
    joined.try_merge(&TextSlice::extent(store.clone(), 2, 0..7)).unwrap();
    assert!(loro::text_copy_metrics().is_empty(), "merge copies metadata only");
    assert_eq!(left.backing_parts()[0].range, 0..2);
    assert_eq!(joined.backing_parts().iter().map(|p|p.extent).collect::<Vec<_>>(), vec![Some(1),Some(2)]);
    let clipped = joined.slice_clone(1..5);
    assert_eq!(clipped.backing_parts().iter().map(|p|(p.extent,p.range.clone())).collect::<Vec<_>>(), vec![(Some(1),1..2),(Some(2),0..3)]);
    assert!(loro::text_copy_metrics().is_empty(), "byte clipping resolves no payload");
    clipped.try_with_bytes("test.contiguous", |bytes| assert_eq!(bytes, "b玉".as_bytes())).unwrap();
    let counts = loro::text_copy_metrics();
    assert_eq!(counts["text_slice.extent_sequence_join"].extent_bytes, 4);
    assert_eq!(counts["text_slice.extent_sequence_join"].extent_calls, 2);
    assert_eq!(clipped.materialize("test.materialize").as_ref(), "b玉".as_bytes());
    assert_eq!(loro::text_copy_metrics()["text_slice.extent_sequence_join"].extent_bytes, 8);
    let mut different = TextSlice::extent(store.clone(), 3, 0..2);
    different.try_merge(&TextSlice::extent(store.clone(), 2, 0..7)).unwrap();
    assert!(!joined.ptr_eq(&different));
    assert_ne!(joined, different, "same resolver and length do not imply identity or equal bytes");
    let other = Arc::new(Store::default());
    other.0.write().unwrap().insert(1,b"ab".to_vec());
    assert!(!left.can_merge(&TextSlice::extent(other,1,0..2)), "numeric extent IDs are store-local");
    assert!(!left.can_merge(&TextSlice::from_bytes(b"c")), "mixed native/extent must not repack");
    assert_eq!(left.to_vec(), b"ab");
    assert_eq!(joined.to_vec(), "ab玉🙂".as_bytes());
}

#[test]
fn composite_failed_part_never_reaches_consumer_and_nested_views_are_bounded() {
    struct Bad { behavior: u8 }
    impl ExtentResolver for Bad {
        fn with_bytes(&self, id:u64, _:Range<usize>, _: &'static str, visit:&mut dyn FnMut(&[u8])) -> io::Result<()> {
            if id == 1 { visit(b"a"); return Ok(()); }
            match self.behavior {
                0 => Err(io::ErrorKind::NotFound.into()),
                1 => { visit(b""); Ok(()) },
                2 => { visit(b"b"); visit(b"b"); Ok(()) },
                _ => panic!("resolver failure"),
            }
        }
    }
    for behavior in 0..4 {
        let resolver = Arc::new(Bad{behavior});
        let mut slice = TextSlice::extent(resolver.clone(), 1, 0..1);
        slice.try_merge(&TextSlice::extent(resolver, 2, 0..1)).unwrap();
        let mut reached = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| slice.try_with_bytes("test.bad", |_| { reached = true; })));
        assert!(result.is_err() || result.unwrap().is_err());
        assert!(!reached, "no partial valid operand escapes an incomplete sequence");
    }
    let store = Arc::new(Store::default()); store.0.write().unwrap().extend([(1,b"a".to_vec()),(2,b"b".to_vec())]);
    let mut joined = TextSlice::extent(store.clone(),1,0..1);
    joined.try_merge(&TextSlice::extent(store,2,0..1)).unwrap();
    joined.try_with_bytes("outer", |outer| joined.try_with_bytes("inner", |inner| assert_eq!(outer,inner)).unwrap()).unwrap();
}

#[test]
fn three_allocations_keep_cow_source_prefix_and_physical_parts_immutable() {
    let doc = LoroDoc::new(); doc.set_peer_id(92).unwrap();
    let text = doc.get_text("body");
    let mut allocator = Allocator { store: Arc::new(Store::default()), next: 200 };
    start_native_text_observations();
    for (pos, value) in [(0,"a"),(1,"玉"),(2,"🙂")] {
        text.insert_unicode_with_allocator(pos,value,&mut allocator).unwrap();
    }
    doc.commit();
    let events = take_native_text_observations();
    let (count, ops) = events.iter().find_map(|e| match e { NativeTextObservation::Finalize { native_op_count,text_ops,.. } => Some((*native_op_count,text_ops)),_=>None }).unwrap();
    assert_eq!(count,1);
    assert_eq!(ops[0].id,ID::new(92,0));
    let expected = vec![(Some(200),0..1),(Some(201),0..3),(Some(202),0..4)];
    assert_eq!(ops[0].op.backing_parts.iter().map(|p|(p.extent,p.range.clone())).collect::<Vec<_>>(), expected);
    assert_eq!(ops[0].original_ranges,ops[0].op.backing_parts);
    let original_parts = doc.inspect_text_op_backings();
    assert_eq!(original_parts.iter().map(|p|(p.extent,p.range.clone())).collect::<Vec<_>>(), expected);
    let branch = doc.fork_cow().unwrap();
    branch.get_text("body").insert(1,"X").unwrap(); branch.commit();
    assert_eq!(branch.get_text("body").to_string(),"aX玉🙂");
    assert_eq!(text.to_string(),"a玉🙂");
    assert_eq!(doc.inspect_text_op_backings(),original_parts);
    let snapshot = doc.inspect_text_backings();
    assert!(snapshot.state.iter().all(|part|part.kind == "extent"));
    assert_eq!(snapshot.state.iter().map(|p|(p.extent,p.range.clone())).collect::<Vec<_>>(), expected);
    doc.checkout(&loro::Frontiers::from_id(ID::new(92,0))).unwrap();
    assert_eq!(text.to_string(),"a");
    doc.checkout_to_latest();
    assert_eq!(text.to_string(),"a玉🙂");
    drop(text); drop(doc); drop(allocator);
    assert_eq!(branch.get_text("body").to_string(),"aX玉🙂");
}
