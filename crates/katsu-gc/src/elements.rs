//! Indexed properties: the values a program reaches with `a[0]` rather than `a.x`.
//!
//! Every property in `ordinary.rs` is a name in a shape and a value in a slot the shape points at.
//! That is the right arrangement for `a.x` and the wrong one for `a[0]`. A shape is shared between
//! objects with the same properties in the same order, and no two arrays have the same indices in
//! the same order for long, so putting indices in a shape means a transition tree that grows one
//! node per element and is shared with nothing. It also means every read of `a[i]` turns the integer
//! into a string first, and interns it, to look up a name that was an integer all along.
//!
//! So indices do not go in the shape. They go in a separate array hanging off the object, addressed
//! by the integer directly, and reading one is a bounds check and a load.
//!
//! # The layout
//!
//! ```text
//! [kind][capacity][value 0][value 1]...
//! ```
//!
//! The same shape as the properties overflow array next door, and for the same reason: there are no
//! names in it, because the index is the name. Eight bytes of header, so the first value lands at an
//! eight byte offset inside an eight byte aligned allocation and every value after it is aligned
//! too.
//!
//! # Why there is no length in here
//!
//! Because the two callers want two different lengths and neither of them is this one. An array's
//! `length` is an own property of the array, it is writable, and `a.length = 100` has to work
//! without allocating room for a hundred values. An ordinary object with `o[5] = 1` on it has no
//! length at all, and what enumeration wants from it is the set of indices that are really there,
//! which means skipping holes over the capacity whether a length is recorded or not. A length here
//! would be a third number that agrees with the other two most of the time, which is a bug waiting
//! for the case where it does not.
//!
//! # The hole is free
//!
//! A hole is the value crate's `Value::EMPTY`, whose bit pattern is zero, and the bump heap hands
//! out committed pages that are already zero and never reuses memory. So a freshly allocated
//! elements array is entirely holes without a single store, and growing one by copying the old
//! values into a larger allocation leaves the new tail as holes for the same reason. `[1, , 3]` and
//! `new Array(1000)` both cost exactly what their values cost.
//!
//! This crate does not know what a `Value` is, and does not need to. It moves eight byte words
//! around and the one thing it knows about their contents is that the two smallest numbers are not
//! values: zero means nothing was ever written here, and one means the property is on this object
//! under the text of the number instead. Zero meaning nothing is the same thing `ordinary.rs`
//! already relies on for its properties word.

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};
use crate::object::{HeapKind, read_u32, read_u64, slot_of, write_kind, write_u32, write_u64};

/// Bytes before the first value: the kind tag and the capacity.
pub(crate) const ELEMENTS_HEADER: usize = 8;
/// How many values this array has room for.
const CAPACITY_OFFSET: usize = 4;
/// Bytes per value, matching the property slots so that one value is one width everywhere.
const VALUE_SIZE: usize = 8;

/// What an index reads back as when nothing was ever written there.
///
/// Zero, which is the value crate's empty. Stated here as a constant rather than spelled `0` at
/// each use, because the three places that compare against it are making a claim about the value
/// encoding and not about arithmetic.
pub const HOLE: u64 = 0;

/// What an index reads back as when the property is here but its value is not.
///
/// One, which is not a value any encoding produces, so it cannot be confused with one. It means the
/// index is a property of this object under the text of the number instead, and the reader has to go
/// and look for it there. Two things put it in: an index too sparse for an array, once the array has
/// grown out past it, and a property defined at an index with flags the storage here cannot hold.
/// Both are rare, and what they have in common is that the alternative is the same property in two
/// places at once, which answers differently depending on which one the reader happened to look at.
pub const NAMED: u64 = 1;

/// Whether these bits are a value rather than one of the two words that stand in for the absence of
/// one.
///
/// One comparison rather than two, which is why [`HOLE`] and [`NAMED`] are the two smallest numbers
/// there are. This is on the read path of every `a[i]` in every program, so the shape of it matters.
#[must_use]
pub const fn is_value(bits: u64) -> bool {
    bits > NAMED
}

/// How many values the first elements array holds.
///
/// Four, matching the first properties array, and for the same reason: an object that has just
/// grown an indexed property by assignment is almost always in the middle of a loop that is about
/// to add more.
pub(crate) const FIRST_ELEMENTS: u32 = 4;

/// How far past the end a write may reach before element storage stops being worth it.
///
/// Storing `o[5] = 1` on an empty object costs six slots to hold one value, which is fine. Storing
/// `o[100000] = 1` costs eight hundred kilobytes to hold one value, which is not. Past this gap the
/// index goes in the shape under the text of the number instead, where it costs what one property
/// costs.
///
/// The number is a policy choice and not a measurement. A thousand and twenty four slots is eight
/// kilobytes of slack, which is small enough that the worst case is not worth worrying about and
/// large enough that a loop counting up with a few gaps in it never crosses the line. When there
/// are real programs to measure, measure it.
pub const MAX_GAP: u32 = 1024;

/// What happened to a write of an indexed property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// The value is in the elements array.
    Yes,
    /// The index is too far past the end for element storage to pay, and the caller should put the
    /// value under the text of the number instead.
    TooSparse,
    /// The index is already a property of this object under the text of the number, so the write
    /// belongs there and not here. Same answer as [`Stored::TooSparse`] for the caller and a
    /// different reason, and they are told apart because only one of them says anything about how
    /// much room an array would take.
    Named,
    /// The heap is full, or the array the write asked for would be larger than the cage.
    NoRoom,
}

/// The indexed properties of an object.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ElementsRef(Slot);

impl std::fmt::Debug for ElementsRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "elements@{:?}", self.0)
    }
}

impl ElementsRef {
    /// Allocate an array with room for `capacity` values, all of them holes.
    ///
    /// Returns `None` if the heap is full or the array would be larger than the cage.
    #[must_use]
    pub fn new(heap: &mut BumpHeap, capacity: u32) -> Option<ElementsRef> {
        let bytes = ELEMENTS_HEADER.checked_add((capacity as usize).checked_mul(VALUE_SIZE)?)?;
        let pointer = heap.allocate(bytes, ObjectKind::Elements)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `bytes` long, which covers the header. The values are left as
        // the zeroes the heap hands out, which read back as holes.
        unsafe {
            write_kind(pointer, HeapKind::Elements);
            write_u32(pointer, CAPACITY_OFFSET, capacity);
        }
        Some(ElementsRef(slot))
    }

    /// How many values this array has room for.
    #[must_use]
    pub fn capacity(self, cage: &Cage) -> u32 {
        // SAFETY: the slot points at an elements array, whose header is `ELEMENTS_HEADER` bytes.
        unsafe { read_u32(cage, self.offset(), CAPACITY_OFFSET) }
    }

    /// The value at `index`, or [`HOLE`] if the index is past the end or nothing was written there.
    ///
    /// The bounds check is here rather than left to the caller because this is the read half of
    /// `a[i]` and an index out of range is the ordinary case rather than a mistake: a program that
    /// walks past the end of an array gets `undefined` and carries on.
    #[must_use]
    pub fn value_at(self, cage: &Cage, index: u32) -> u64 {
        if index >= self.capacity(cage) {
            return HOLE;
        }
        // SAFETY: the index was just checked against the capacity in the header.
        unsafe {
            read_u64(
                cage.address_of(self.offset()),
                ELEMENTS_HEADER + (index as usize) * VALUE_SIZE,
            )
        }
    }

    /// Write the value at `index`, which the caller has checked against the capacity.
    ///
    /// Writing past the capacity is not unsafe, it is refused, because the alternative is scribbling
    /// over whatever the heap put after this array. Growing is [`ObjectRef::set_element`]'s job,
    /// because it is the one holding the word that has to point at the larger array afterwards.
    ///
    /// [`ObjectRef::set_element`]: crate::ObjectRef::set_element
    pub fn set(self, heap: &mut BumpHeap, index: u32, value: u64) {
        if index >= self.capacity(heap.cage()) {
            return;
        }
        // SAFETY: as `value_at`, and `&mut BumpHeap` means nothing else is reading the cage.
        unsafe {
            write_u64(
                heap.cage().address_of(self.offset()),
                ELEMENTS_HEADER + (index as usize) * VALUE_SIZE,
                value,
            );
        }
    }

    /// One past the highest index that holds a value, or `None` when every slot is a hole.
    ///
    /// A scan, because there is no length in the header and this is what the absence of one costs.
    /// Nothing hot asks: it is here for enumeration and for printing, both of which are already
    /// walking every element to decide what to say about it.
    #[must_use]
    pub fn used(self, cage: &Cage) -> Option<u32> {
        (0..self.capacity(cage))
            .rev()
            .find(|&index| is_value(self.value_at(cage, index)))
            .map(|index| index + 1)
    }

    /// The slot this array lives at, for putting it in an object's header word.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read an elements array back out of a slot, or `None` if the slot is not a pointer.
    ///
    /// A zero in an object's elements word is not a pointer, which is what makes "this object has no
    /// indexed properties" the state a freshly allocated object is already in.
    #[must_use]
    pub const fn from_slot(slot: Slot) -> Option<ElementsRef> {
        if slot.is_pointer() {
            Some(ElementsRef(slot))
        } else {
            None
        }
    }

    /// How large an array has to be to hold `index`, or `None` if it is not worth building.
    ///
    /// Doubling, so that a loop appending one element at a time allocates a logarithmic number of
    /// times rather than a linear one, and never smaller than the index needs.
    pub(crate) fn grown_for(capacity: u32, index: u32) -> Option<u32> {
        if index.saturating_sub(capacity) > MAX_GAP {
            return None;
        }
        let wanted = index.checked_add(1)?;
        Some(capacity.saturating_mul(2).max(wanted).max(FIRST_ELEMENTS))
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{ELEMENTS_HEADER, ElementsRef, HOLE, MAX_GAP, NAMED, is_value};
    use crate::bump::BumpHeap;
    use crate::cage::Slot;
    use crate::object::HeapKind;

    fn heap() -> BumpHeap {
        BumpHeap::new().expect("should reserve a cage")
    }

    #[test]
    fn a_fresh_array_is_all_holes_and_nobody_wrote_them() {
        let mut heap = heap();
        let elements = ElementsRef::new(&mut heap, 8).expect("should have room");
        for index in 0..8 {
            assert_eq!(elements.value_at(heap.cage(), index), HOLE);
        }
        assert_eq!(elements.used(heap.cage()), None);
    }

    #[test]
    fn a_value_that_was_put_in_comes_back_out() {
        let mut heap = heap();
        let elements = ElementsRef::new(&mut heap, 4).expect("should have room");
        elements.set(&mut heap, 0, 11);
        elements.set(&mut heap, 3, 44);
        assert_eq!(elements.value_at(heap.cage(), 0), 11);
        assert_eq!(elements.value_at(heap.cage(), 1), HOLE);
        assert_eq!(elements.value_at(heap.cage(), 3), 44);
    }

    #[test]
    fn reading_past_the_end_is_a_hole_rather_than_a_crash() {
        let mut heap = heap();
        let elements = ElementsRef::new(&mut heap, 2).expect("should have room");
        elements.set(&mut heap, 0, 11);
        assert_eq!(elements.value_at(heap.cage(), 2), HOLE);
        assert_eq!(elements.value_at(heap.cage(), u32::MAX), HOLE);
    }

    #[test]
    fn writing_past_the_end_is_refused_rather_than_scribbling() {
        let mut heap = heap();
        let first = ElementsRef::new(&mut heap, 2).expect("should have room");
        let second = ElementsRef::new(&mut heap, 2).expect("should have room");
        first.set(&mut heap, 2, 99);
        first.set(&mut heap, 3, 99);
        assert_eq!(second.value_at(heap.cage(), 0), HOLE);
        assert_eq!(second.value_at(heap.cage(), 1), HOLE);
    }

    #[test]
    fn the_used_length_ignores_the_holes_after_it_and_keeps_the_ones_before() {
        let mut heap = heap();
        let elements = ElementsRef::new(&mut heap, 8).expect("should have room");
        elements.set(&mut heap, 0, 11);
        elements.set(&mut heap, 4, 55);
        assert_eq!(elements.used(heap.cage()), Some(5));
    }

    #[test]
    fn the_two_words_that_are_not_values_are_the_two_smallest_numbers() {
        // The whole point of the pair, because it makes the test on the read path one comparison.
        // If either of these ever moves, `is_value` has to stop being a single `>`.
        assert_eq!(HOLE, 0);
        assert_eq!(NAMED, 1);
        assert!(!is_value(HOLE));
        assert!(!is_value(NAMED));
        assert!(is_value(2));
    }

    #[test]
    fn an_elements_array_is_told_apart_from_everything_else_in_the_cage() {
        let mut heap = heap();
        let elements = ElementsRef::new(&mut heap, 1).expect("should have room");
        assert_eq!(
            HeapKind::of(heap.cage(), elements.slot()),
            Some(HeapKind::Elements)
        );
    }

    #[test]
    fn an_empty_word_is_not_an_elements_array() {
        assert!(ElementsRef::from_slot(Slot::from_bits(0)).is_none());
    }

    #[test]
    fn an_array_costs_its_header_and_its_values_and_nothing_else() {
        let mut heap = heap();
        let before = heap.cursor();
        ElementsRef::new(&mut heap, 4).expect("should have room");
        assert_eq!(heap.cursor() - before, ELEMENTS_HEADER + 4 * 8);
    }

    #[test]
    fn growing_doubles_until_the_index_needs_more() {
        assert_eq!(ElementsRef::grown_for(0, 0), Some(4));
        assert_eq!(ElementsRef::grown_for(4, 4), Some(8));
        assert_eq!(ElementsRef::grown_for(8, 8), Some(16));
        assert_eq!(ElementsRef::grown_for(4, 100), Some(101));
    }

    #[test]
    fn a_gap_wider_than_the_limit_is_not_worth_an_array() {
        assert_eq!(ElementsRef::grown_for(0, MAX_GAP), Some(MAX_GAP + 1));
        assert_eq!(ElementsRef::grown_for(0, MAX_GAP + 1), None);
        assert_eq!(ElementsRef::grown_for(0, u32::MAX), None);
    }
}
