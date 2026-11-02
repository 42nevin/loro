//! Storage modes for the tree. Owned remains the unrestricted default.
//! Cow is explicit: its element and cache values must be immutable through
//! shared references, as specified by `CowBTreeTrait`.
use crate::{BTreeTrait, CowBTreeTrait, LeafNode, Node};
use std::sync::Arc;
use thunderdome::Index;

pub const PAGE_SIZE: usize = 64;

mod sealed {
    pub trait Sealed {}
}

pub struct Owned;
pub struct Cow;
impl sealed::Sealed for Owned {}
impl sealed::Sealed for Cow {}

pub trait StorageMode<B: BTreeTrait>: sealed::Sealed {
    type Internal: ArenaStorage<Node<B>>;
    type Leaves: ArenaStorage<LeafNode<B::Elem>>;
}
impl<B: BTreeTrait> StorageMode<B> for Owned {
    type Internal = thunderdome::Arena<Node<B>>;
    type Leaves = thunderdome::Arena<LeafNode<B::Elem>>;
}
impl<B: CowBTreeTrait> StorageMode<B> for Cow {
    type Internal = PagedArena<Node<B>>;
    type Leaves = PagedArena<LeafNode<B::Elem>>;
}

pub trait ArenaStorage<T>: Sized {
    fn new() -> Self;
    fn len(&self) -> usize;
    fn reserve(&mut self, additional: usize);
    fn insert(&mut self, value: T) -> Index;
    fn remove(&mut self, index: Index) -> Option<T>;
    fn get(&self, index: Index) -> Option<&T>;
    fn get_mut(&mut self, index: Index) -> Option<&mut T>;
    fn get2_mut(&mut self, a: Index, b: Index) -> (Option<&mut T>, Option<&mut T>);
    fn contains(&self, index: Index) -> bool;
    fn iter<'a>(&'a self) -> impl Iterator<Item = (Index, &'a T)>
    where
        T: 'a;
}
impl<T> ArenaStorage<T> for thunderdome::Arena<T> {
    fn new() -> Self {
        Self::new()
    }
    fn len(&self) -> usize {
        self.len()
    }
    fn reserve(&mut self, n: usize) {
        self.reserve(n)
    }
    fn insert(&mut self, value: T) -> Index {
        self.insert(value)
    }
    fn remove(&mut self, index: Index) -> Option<T> {
        self.remove(index)
    }
    fn get(&self, index: Index) -> Option<&T> {
        self.get(index)
    }
    fn get_mut(&mut self, index: Index) -> Option<&mut T> {
        self.get_mut(index)
    }
    fn get2_mut(&mut self, a: Index, b: Index) -> (Option<&mut T>, Option<&mut T>) {
        self.get2_mut(a, b)
    }
    fn contains(&self, index: Index) -> bool {
        self.contains(index)
    }
    fn iter<'a>(&'a self) -> impl Iterator<Item = (Index, &'a T)>
    where
        T: 'a,
    {
        self.iter()
    }
}

#[derive(Clone)]
enum Entry<T> {
    Occupied { generation: u32, value: T },
    Vacant { generation: u32, next: Option<u32> },
}
impl<T> Entry<T> {
    fn get(&self, generation: u32) -> Option<&T> {
        match self {
            Self::Occupied {
                generation: g,
                value,
            } if *g == generation => Some(value),
            _ => None,
        }
    }
    fn get_mut(&mut self, generation: u32) -> Option<&mut T> {
        match self {
            Self::Occupied {
                generation: g,
                value,
            } if *g == generation => Some(value),
            _ => None,
        }
    }
}
struct Page<T>(Vec<Entry<T>>);
impl<T: Clone> Clone for Page<T> {
    fn clone(&self) -> Self {
        #[cfg(feature = "clone_work")]
        page_work::page::<T>(
            self.0.len(),
            self.0
                .iter()
                .filter(|e| matches!(e, Entry::Occupied { .. }))
                .count(),
        );
        Self(self.0.clone())
    }
}
struct PageRef<T>(Arc<Page<T>>);
impl<T> Clone for PageRef<T> {
    fn clone(&self) -> Self {
        #[cfg(feature = "clone_work")]
        page_work::reference();
        Self(self.0.clone())
    }
}

/// Explicit copy-on-write arena. Unlike the default Owned storage, callers must
/// not mutate values through shared references. Only the tree's audited Cow
/// mode uses this storage. Slot identities and the free-list order are stable.
pub struct PagedArena<T: Clone> {
    pages: im::Vector<PageRef<T>>,
    slots: usize,
    len: usize,
    first_free: Option<u32>,
}
impl<T: Clone> Clone for PagedArena<T> {
    fn clone(&self) -> Self {
        Self {
            pages: self.pages.clone(),
            slots: self.slots,
            len: self.len,
            first_free: self.first_free,
        }
    }
}
fn index(slot: u32, generation: u32) -> Index {
    Index::from_bits((u64::from(generation) << 32) | u64::from(slot)).expect("nonzero generation")
}
impl<T: Clone> PagedArena<T> {
    fn entry(&self, slot: usize) -> Option<&Entry<T>> {
        self.pages.get(slot / PAGE_SIZE)?.0 .0.get(slot % PAGE_SIZE)
    }
    fn entry_mut(&mut self, slot: usize) -> Option<&mut Entry<T>> {
        if slot >= self.slots {
            return None;
        }
        let page = &mut self.pages.get_mut(slot / PAGE_SIZE)?.0;
        Arc::make_mut(page).0.get_mut(slot % PAGE_SIZE)
    }
}
impl<T: Clone> ArenaStorage<T> for PagedArena<T> {
    fn new() -> Self {
        Self {
            pages: im::Vector::new(),
            slots: 0,
            len: 0,
            first_free: None,
        }
    }
    fn len(&self) -> usize {
        self.len
    }
    // The sole tree caller uses this as an optional FromIterator allocation
    // hint. Persistent pages grow on demand; no old slots are visited here.
    fn reserve(&mut self, _additional: usize) {}
    fn insert(&mut self, value: T) -> Index {
        let len = self
            .len
            .checked_add(1)
            .filter(|n| *n <= u32::MAX as usize)
            .expect("Cannot insert more than u32::MAX elements into Arena");
        let result = if let Some(slot) = self.first_free {
            let (generation, next) = match self.entry(slot as usize).expect("free slot exists") {
                Entry::Vacant { generation, next } => {
                    (generation.checked_add(1).unwrap_or(1), *next)
                }
                _ => panic!("free slot is occupied"),
            };
            *self.entry_mut(slot as usize).expect("free slot exists") =
                Entry::Occupied { generation, value };
            self.first_free = next;
            index(slot, generation)
        } else {
            let slot = u32::try_from(self.slots).expect("Arena storage exceeded u32 slots");
            if self.slots % PAGE_SIZE == 0 {
                self.pages
                    .push_back(PageRef(Arc::new(Page(Vec::with_capacity(PAGE_SIZE)))));
            }
            let page = &mut self.pages.back_mut().expect("last page").0;
            Arc::make_mut(page).0.push(Entry::Occupied {
                generation: 1,
                value,
            });
            self.slots += 1;
            index(slot, 1)
        };
        self.len = len;
        result
    }
    fn remove(&mut self, id: Index) -> Option<T> {
        self.get(id)?;
        let next = self.first_free;
        let entry = self.entry_mut(id.slot() as usize).expect("checked slot");
        let old = std::mem::replace(
            entry,
            Entry::Vacant {
                generation: id.generation(),
                next,
            },
        );
        self.first_free = Some(id.slot());
        self.len -= 1;
        match old {
            Entry::Occupied { value, .. } => Some(value),
            _ => unreachable!(),
        }
    }
    fn get(&self, id: Index) -> Option<&T> {
        self.entry(id.slot() as usize)?.get(id.generation())
    }
    fn get_mut(&mut self, id: Index) -> Option<&mut T> {
        self.get(id)?;
        self.entry_mut(id.slot() as usize)?.get_mut(id.generation())
    }
    fn get2_mut(&mut self, a: Index, b: Index) -> (Option<&mut T>, Option<&mut T>) {
        assert!(
            a != b,
            "Arena::get2_mut is called with two identical indices"
        );
        if a.slot() == b.slot() {
            return if self.get(a).is_some() {
                (self.get_mut(a), None)
            } else {
                (None, self.get_mut(b))
            };
        }
        let (low, high, reversed) = if a.slot() < b.slot() {
            (a, b, false)
        } else {
            (b, a, true)
        };
        let hi = high.slot() as usize;
        let lo = low.slot() as usize;
        // Match the existing arena's split_at_mut boundary behavior.
        assert!(hi <= self.slots, "arena split index out of bounds");
        if hi == self.slots {
            let value = self.get_mut(low);
            return if reversed {
                (None, value)
            } else {
                (value, None)
            };
        }
        let (left, right) = if lo / PAGE_SIZE == hi / PAGE_SIZE {
            let page = &mut self.pages.get_mut(lo / PAGE_SIZE).expect("page").0;
            let (left, right) = Arc::make_mut(page).0.split_at_mut(hi % PAGE_SIZE);
            (left.get_mut(lo % PAGE_SIZE), right.first_mut())
        } else {
            let (left, right) = self.pages.focus_mut().split_at(hi / PAGE_SIZE);
            let left = left
                .narrow(lo / PAGE_SIZE..lo / PAGE_SIZE + 1)
                .into_iter()
                .next()
                .expect("low page");
            let right = right.into_iter().next().expect("high page");
            (
                Arc::make_mut(&mut left.0).0.get_mut(lo % PAGE_SIZE),
                Arc::make_mut(&mut right.0).0.get_mut(hi % PAGE_SIZE),
            )
        };
        let left = left.and_then(|e| e.get_mut(low.generation()));
        let right = right.and_then(|e| e.get_mut(high.generation()));
        if reversed {
            (right, left)
        } else {
            (left, right)
        }
    }
    fn contains(&self, id: Index) -> bool {
        self.get(id).is_some()
    }
    fn iter<'a>(&'a self) -> impl Iterator<Item = (Index, &'a T)>
    where
        T: 'a,
    {
        self.pages
            .iter()
            .flat_map(|page| page.0 .0.iter())
            .enumerate()
            .filter_map(|(slot, entry)| match entry {
                Entry::Occupied { generation, value } => {
                    Some((index(slot as u32, *generation), value))
                }
                _ => None,
            })
    }
}

#[cfg(feature = "clone_work")]
pub mod page_work {
    use std::{cell::RefCell, collections::BTreeMap};
    #[derive(Default, Debug)]
    pub struct Counts {
        pub pages: usize,
        pub slots: usize,
        pub occupied: usize,
        pub vacant: usize,
    }
    #[derive(Default, Debug)]
    pub struct Work {
        pub by_type: BTreeMap<&'static str, Counts>,
        pub references: usize,
    }
    thread_local! { static WORK: RefCell<Option<Work>> = const { RefCell::new(None) }; }
    pub fn reset() {
        WORK.with(|w| *w.borrow_mut() = Some(Work::default()));
    }
    pub fn take() -> Work {
        WORK.with(|w| w.borrow_mut().take().unwrap_or_default())
    }
    pub(super) fn reference() {
        WORK.with(|w| {
            if let Some(w) = w.borrow_mut().as_mut() {
                w.references += 1;
            }
        });
    }
    pub(super) fn page<T>(slots: usize, occupied: usize) {
        WORK.with(|w| {
            if let Some(w) = w.borrow_mut().as_mut() {
                let c = w.by_type.entry(std::any::type_name::<T>()).or_default();
                c.pages += 1;
                c.slots += slots;
                c.occupied += occupied;
                c.vacant += slots - occupied;
            }
        });
    }
}
