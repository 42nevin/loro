#![cfg(feature = "shared_arena_experiment")]
//! Provider-only grouping control: immutable preseeded adjacent ranges let the
//! real RLE implementation merge. This is not an app allocator adoption proof.
use loro::{ExtentResolver, LoroDoc, TextExtentAllocator, TextSlice, ExportMode, VersionVector};
use loro::native_text_observation::{start_native_text_observations, take_native_text_observations, NativeTextObservation};
use std::{io, ops::Range, sync::Arc};
struct Memory;
impl ExtentResolver for Memory {
    fn with_bytes(&self, extent: u64, range: Range<usize>, _: &'static str,
        visit: &mut dyn FnMut(&[u8])) -> io::Result<()> {
        let bytes = match extent { 11 => "a玉".as_bytes(), 12 => "🙂".as_bytes(), _ => return Err(io::ErrorKind::NotFound.into()) };
        visit(bytes.get(range).ok_or(io::ErrorKind::InvalidInput)?);
        Ok(())
    }
}
struct Allocator { resolver: Arc<Memory>, extent: u64, range: Range<usize> }
impl TextExtentAllocator for Allocator {
    fn allocate_text(&mut self, len: usize, generate: &mut dyn FnMut(&mut [u8])) -> io::Result<TextSlice> {
        let mut bytes = vec![0; len];
        generate(&mut bytes);
        self.resolver.with_bytes(self.extent, self.range.clone(), "test.verify", &mut |expected| assert_eq!(expected, bytes))?;
        Ok(TextSlice::extent(self.resolver.clone(), self.extent, self.range.clone()))
    }
}
#[test]
fn actual_native_grouping_and_physical_split_are_observed() {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    let text = doc.get_text("text");
    let resolver = Arc::new(Memory);
    loro::reset_text_copy_metrics();
    start_native_text_observations();
    for (pos, value, extent, range) in [(0,"a",11,0..1),(1,"玉",11,1..4),(2,"🙂",12,0..4)] {
        text.insert_unicode_with_allocator(pos, value, &mut Allocator { resolver: resolver.clone(), extent, range }).unwrap();
    }
    let copies = loro::text_copy_metrics();
    let input = copies.get("allocated.input_string").expect("actual input-copy closure ran");
    assert_eq!(input.native_calls, 3);
    assert_eq!(input.native_bytes, 8); // a (1), 玉 (3), 🙂 (4)
    assert_eq!(input.extent_calls, 0);
    assert_eq!(input.extent_bytes, 0);
    doc.commit();
    assert_eq!(text.to_string(), "a玉🙂");
    // A real partial native export exercises Op::slice on a scalar boundary.
    let mut from = VersionVector::default();
    from.set_last(loro::ID::new(7, 0));
    doc.export(ExportMode::updates(&from)).unwrap();
    let events = take_native_text_observations();
    let insertions: Vec<_> = events.iter().filter_map(|e| match e { NativeTextObservation::Insert { context, op } => Some((context,op)), _ => None }).collect();
    assert_eq!(insertions.len(), 3);
    assert_eq!(insertions[1].0.id, loro::ID::new(7,1));
    assert_eq!(insertions[1].0.unicode_pos, 1);
    assert_eq!(insertions[1].0.entity_pos, 1);
    assert_eq!(insertions[1].1.backing.range, 1..4);
    assert!(events.iter().any(|e| matches!(e, NativeTextObservation::Merge { left, right, result }
        if left.counter == 0 && right.counter == 1 && result.scalar_count == 2 && result.backing.range == (0..4))));
    let finalized = events.iter().find_map(|e| match e { NativeTextObservation::Finalize { native_op_count, text_ops, .. } => Some((*native_op_count,text_ops)), _ => None }).unwrap();
    assert_eq!(finalized.0, 2);
    assert_eq!(finalized.1[0].original_ranges.len(), 2);
    assert_eq!(finalized.1[0].original_ranges[0].range, 0..1);
    assert_eq!(finalized.1[0].original_ranges[1].range, 1..4);
    assert_eq!(finalized.1[1].original_ranges[0].extent, Some(12));
    assert!(events.iter().any(|e| matches!(e, NativeTextObservation::Slice { result, .. }
        if result.counter == 1 && result.backing.extent == Some(11) && result.backing.range == (1..4))));
    assert!(take_native_text_observations().is_empty());
}
