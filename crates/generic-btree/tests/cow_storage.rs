//! Explicit owned/shared storage contracts; no allocator or timing assertions.
use generic_btree::rle::{CanRemove, HasLength, Mergeable, Sliceable, TryInsert};
use generic_btree::storage::{ArenaStorage, PagedArena, PAGE_SIZE};
use generic_btree::{BTree, BTreeTrait, Child, CowBTree, CowBTreeTrait};
use std::{cell::Cell, marker::PhantomData, ops::Range};

#[derive(Debug, Clone)]
struct CellItem(Cell<i32>);
impl CellItem {
    fn new(v: i32) -> Self {
        Self(Cell::new(v))
    }
    fn value(&self) -> i32 {
        self.0.get()
    }
}
#[derive(Debug)]
struct NoCloneItem(i32);
impl NoCloneItem {
    fn new(v: i32) -> Self {
        Self(v)
    }
    fn value(&self) -> i32 {
        self.0
    }
}
#[derive(Debug, Clone)]
struct Plain(i32);
impl Plain {
    fn new(v: i32) -> Self {
        Self(v)
    }
    fn value(&self) -> i32 {
        self.0
    }
}
macro_rules! element {
    ($t:ty) => {
        impl HasLength for $t {
            fn rle_len(&self) -> usize {
                1
            }
        }
        impl Sliceable for $t {
            fn _slice(&self, range: Range<usize>) -> Self {
                assert_eq!(range, 0..1);
                Self::new(self.value())
            }
        }
        impl Mergeable for $t {
            fn can_merge(&self, _: &Self) -> bool {
                false
            }
            fn merge_right(&mut self, _: &Self) {
                panic!("not mergeable")
            }
            fn merge_left(&mut self, _: &Self) {
                panic!("not mergeable")
            }
        }
        impl TryInsert for $t {
            fn try_insert(&mut self, _: usize, elem: Self) -> Result<(), Self> {
                Err(elem)
            }
        }
        impl CanRemove for $t {
            fn can_remove(&self) -> bool {
                false
            }
        }
    };
}
element!(CellItem);
element!(NoCloneItem);
element!(Plain);
struct Tree<E>(PhantomData<E>);
impl<E> BTreeTrait for Tree<E>
where
    E: std::fmt::Debug + HasLength + Sliceable + Mergeable + TryInsert + CanRemove,
{
    type Elem = E;
    type Cache = usize;
    type CacheDiff = isize;
    const USE_DIFF: bool = false;
    fn calc_cache_internal(cache: &mut usize, children: &[Child<Self>]) -> isize {
        let old = *cache;
        *cache = children.iter().map(|c| *c.cache()).sum();
        *cache as isize - old as isize
    }
    fn apply_cache_diff(cache: &mut usize, diff: &isize) {
        *cache = (*cache as isize + diff) as usize;
    }
    fn merge_cache_diff(a: &mut isize, b: &isize) {
        *a += b;
    }
    fn get_elem_cache(_: &E) -> usize {
        1
    }
    fn new_cache_to_diff(cache: &usize) -> isize {
        *cache as isize
    }
    fn sub_cache(a: &usize, b: &usize) -> isize {
        *a as isize - *b as isize
    }
}
// Plain has no interior mutation; shared nodes/caches are audited immutable values.
impl CowBTreeTrait for Tree<Plain> {}

#[test]
fn default_owned_keeps_nonclone_elements_and_cell_clone_isolation_and_send() {
    fn send<T: Send>() {}
    send::<BTree<Tree<NoCloneItem>>>();
    send::<BTree<Tree<CellItem>>>();
    let mut nonclone = BTree::<Tree<NoCloneItem>>::new();
    let cursor = nonclone.push(NoCloneItem(7));
    assert_eq!(nonclone.get_elem(cursor.leaf).unwrap().0, 7);
    assert_eq!(nonclone.remove_leaf(cursor).unwrap().0, 7);
    assert!(nonclone.is_empty());
    let mut original = BTree::<Tree<CellItem>>::new();
    let cursor = original.push(CellItem::new(11));
    let other = original.clone();
    other.get_elem(cursor.leaf).unwrap().0.set(99);
    assert_eq!(original.get_elem(cursor.leaf).unwrap().0.get(), 11);
    assert_eq!(other.get_elem(cursor.leaf).unwrap().0.get(), 99);
    std::thread::spawn(move || assert_eq!(original.get_elem(cursor.leaf).unwrap().0.get(), 11))
        .join()
        .unwrap();
}

#[test]
fn shared_tree_keeps_cursors_and_owner_values_across_page_growth_and_cancellation() {
    let mut source = CowBTree::<Tree<Plain>>::new_cow();
    let cursors: Vec<_> = (0..257).map(|v| source.push(Plain(v))).collect();
    let mut branch = source.clone();
    for n in [0, 63, 64, 127, 128, 256] {
        branch.get_elem_mut(cursors[n].leaf).unwrap().0 = -(n as i32) - 1;
        assert_eq!(source.get_elem(cursors[n].leaf).unwrap().0, n as i32);
    }
    source.get_elem_mut(cursors[129].leaf).unwrap().0 = 700;
    assert_eq!(branch.get_elem(cursors[129].leaf).unwrap().0, 129);
    assert_eq!(branch.remove_leaf(cursors[64]).unwrap().0, -65);
    assert!(branch.get_elem(cursors[64].leaf).is_none());
    assert_eq!(source.get_elem(cursors[64].leaf).unwrap().0, 64);
    let replacement = branch.push(Plain(800));
    assert_ne!(replacement.leaf, cursors[64].leaf);
    assert!(branch.get_elem(cursors[64].leaf).is_none());
    assert_eq!(branch.iter().count(), 257);
    drop(branch);
    assert_eq!(source.iter().count(), 257);
    assert_eq!(source.get_elem(cursors[129].leaf).unwrap().0, 700);
    source.clear();
    assert!(source.is_empty());
}

#[test]
fn paged_arena_generations_free_list_and_two_mutable_slots_are_owner_local() {
    assert_eq!(PAGE_SIZE, 64);
    let mut original = PagedArena::<i32>::new();
    original.reserve(257);
    let ids: Vec<_> = (0..257).map(|v| original.insert(v)).collect();
    let mut branch = original.clone();
    for (a, b) in [(1, 2), (63, 64), (128, 256)] {
        let (left, right) = branch.get2_mut(ids[a], ids[b]);
        *left.unwrap() = -1;
        *right.unwrap() = -2;
        assert_eq!(original.get(ids[a]), Some(&(a as i32)));
        assert_eq!(original.get(ids[b]), Some(&(b as i32)));
    }
    assert_eq!(branch.remove(ids[100]), Some(100));
    let new = branch.insert(900);
    assert_ne!(new, ids[100]);
    assert!(!branch.contains(ids[100]));
    assert_eq!(original.get(ids[100]), Some(&100));
    let (stale, fresh) = branch.get2_mut(ids[100], new);
    assert!(stale.is_none());
    *fresh.unwrap() = 901;
    assert_eq!(branch.get(new), Some(&901));
    assert_eq!(branch.len(), 257);
    assert_eq!(branch.iter().count(), 257);
    let held = branch.clone();
    *branch.get_mut(new).unwrap() = 902;
    assert_eq!(held.get(new), Some(&901));
    drop(branch);
    assert_eq!(original.get(ids[100]), Some(&100));
    assert_eq!(held.get(new), Some(&901));
}

#[test]
#[should_panic]
fn paged_arena_rejects_identical_get2_mut_handles() {
    let mut arena = PagedArena::<i32>::new();
    let id = arena.insert(1);
    let _ = arena.get2_mut(id, id);
}

#[cfg(feature = "clone_work")]
#[test]
fn shared_mutation_copies_one_page_not_the_vacant_high_water() {
    use generic_btree::storage::page_work;
    for peak in [64, 1024, 4096] {
        let mut source = PagedArena::new();
        let ids: Vec<_> = (0..peak).map(|i| source.insert(i)).collect();
        for id in &ids[1..] {
            assert!(source.remove(*id).is_some());
        }
        page_work::reset();
        let mut branch = source.clone();
        let fork = page_work::take();
        assert!(
            fork.by_type.is_empty(),
            "fork must not detach any slot page"
        );
        page_work::reset();
        *branch.get_mut(ids[0]).unwrap() = peak;
        let mutation = page_work::take();
        let counts = mutation.by_type.values().collect::<Vec<_>>();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].pages, 1);
        assert_eq!(counts[0].slots, PAGE_SIZE);
        assert_eq!(counts[0].occupied, 1);
        assert_eq!(counts[0].vacant, PAGE_SIZE - 1);
        assert_eq!(source.get(ids[0]), Some(&0));
        assert_eq!(branch.get(ids[0]), Some(&peak));
        eprintln!("PAGED_WORK peak={peak} fork={fork:?} mutation={mutation:?}");
    }
}

#[test]
fn page_detach_clone_panic_keeps_both_owners_and_free_list_usable() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    #[derive(Debug)]
    struct Item {
        value: usize,
        fail: Arc<AtomicBool>,
    }
    impl Clone for Item {
        fn clone(&self) -> Self {
            assert!(
                !self.fail.load(Ordering::Relaxed),
                "deliberate clone failure"
            );
            Self {
                value: self.value,
                fail: self.fail.clone(),
            }
        }
    }
    let fail = Arc::new(AtomicBool::new(false));
    let mut source = PagedArena::new();
    let ids: Vec<_> = (0..130)
        .map(|value| {
            source.insert(Item {
                value,
                fail: fail.clone(),
            })
        })
        .collect();
    assert_eq!(source.remove(ids[5]).unwrap().value, 5);
    let mut branch = source.clone();
    fail.store(true, Ordering::Relaxed);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = branch.insert(Item {
            value: 999,
            fail: fail.clone(),
        });
    }));
    assert!(outcome.is_err());
    fail.store(false, Ordering::Relaxed);
    for owner in [&source, &branch] {
        assert_eq!(owner.len(), 129);
        assert!(!owner.contains(ids[5]));
        for (value, id) in ids.iter().enumerate().filter(|(value, _)| *value != 5) {
            assert_eq!(owner.get(*id).unwrap().value, value);
        }
    }
    let next_source = source.insert(Item {
        value: 1000,
        fail: fail.clone(),
    });
    let next_branch = branch.insert(Item { value: 2000, fail });
    assert_eq!(
        next_source, next_branch,
        "failed detach must preserve generation and free-list head"
    );
    assert_ne!(next_source, ids[5]);
    assert_eq!(source.get(next_source).unwrap().value, 1000);
    assert_eq!(branch.get(next_branch).unwrap().value, 2000);
}
