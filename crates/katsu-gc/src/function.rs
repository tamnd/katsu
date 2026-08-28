//! The objects a function call needs: a closure, an environment, and a function written in Rust.
//!
//! Three of the five kinds the cage holds. The word that tells them apart, and the reads and writes
//! that reach it, are in `object.rs`.
//!
//! # Why a cell is eight bytes and not four
//!
//! Everything else in the cage that holds a value holds a four byte [`Slot`], because that is where
//! the memory goal in `spec/02-the-10x-goal.md` comes from. A context cell holds eight. The reason
//! is that a slot has exactly two things it can be, a thirty one bit integer or a pointer into the
//! cage, and a captured variable can be a double, or `undefined`, or `true`. Those need a heap
//! number and a set of realm singletons to point at, and neither exists before M1. So the choice
//! today is between eight byte cells and a captured `let x = 1.5` that cannot be stored, and the
//! second one is not a choice. This is the one place in the object model where the compression
//! story is not yet paid for, it is written down here rather than discovered later, and it goes back
//! to four bytes in M1 when there is something for a slot to point at.

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};
use crate::object::{HeapKind, read_u32, read_u64, slot_of, write_kind, write_u32, write_u64};
use crate::string::StringRef;

/// Bytes in a closure: the kind tag, the function index, the captured context and the name.
///
/// Sixteen exactly, so there is no alignment padding. The name is in here rather than being looked
/// up through the function index because printing a function has to work after the unit that
/// compiled it is gone, which is the situation an embedder holding a value is always in.
const CLOSURE_SIZE: usize = 16;
/// Which function in the loaded unit this closure runs.
const FUNCTION_OFFSET: usize = 4;
/// The context this closure captured, as raw slot bits, or zero for none.
const CAPTURED_OFFSET: usize = 8;
/// The function's name, as raw slot bits, or zero for an anonymous function.
const NAME_OFFSET: usize = 12;

/// A function value: one blueprint, plus the environment that was live where it was written.
///
/// The blueprint is not in here. Two closures over the same function share one blueprint and differ
/// only in what they captured, which is the whole reason a closure is a separate object from the
/// code it runs. What is in here is an index, because the thing being indexed is a compiled unit
/// owned by the caller rather than anything in the cage.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClosureRef(Slot);

impl std::fmt::Debug for ClosureRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fn@{:?}", self.0)
    }
}

impl ClosureRef {
    /// Stamp out a closure over `function`, capturing `captured`.
    ///
    /// Returns `None` if the heap is full.
    #[must_use]
    pub fn new(
        heap: &mut BumpHeap,
        function: u32,
        captured: Option<ContextRef>,
        name: Option<StringRef>,
    ) -> Option<ClosureRef> {
        let pointer = heap.allocate(CLOSURE_SIZE, ObjectKind::Closure)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `CLOSURE_SIZE` bytes of freshly committed memory that nothing
        // else holds a reference to, and all three writes are inside it.
        unsafe {
            write_kind(pointer, HeapKind::Closure);
            write_u32(pointer, FUNCTION_OFFSET, function);
            write_u32(
                pointer,
                CAPTURED_OFFSET,
                captured.map_or(0, |context| context.0.to_bits()),
            );
            write_u32(
                pointer,
                NAME_OFFSET,
                name.map_or(0, |name| name.slot().to_bits()),
            );
        }
        Some(ClosureRef(slot))
    }

    /// Which function in the loaded unit this closure runs.
    #[must_use]
    pub fn function(self, cage: &Cage) -> u32 {
        // SAFETY: the slot points at a closure, which is `CLOSURE_SIZE` bytes long.
        unsafe { read_u32(cage, self.offset(), FUNCTION_OFFSET) }
    }

    /// The environment this closure captured, or `None` if it captured nothing.
    #[must_use]
    pub fn captured(self, cage: &Cage) -> Option<ContextRef> {
        // SAFETY: as `function`.
        let bits = unsafe { read_u32(cage, self.offset(), CAPTURED_OFFSET) };
        ContextRef::from_slot(Slot::from_bits(bits))
    }

    /// The function's name, or `None` for a function that was written without one.
    #[must_use]
    pub fn name(self, cage: &Cage) -> Option<StringRef> {
        // SAFETY: as `function`.
        let bits = unsafe { read_u32(cage, self.offset(), NAME_OFFSET) };
        StringRef::from_slot(Slot::from_bits(bits))
    }

    /// The slot this closure lives at, for writing into a value or another object.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read a closure back out of a slot, or `None` if the slot is not a pointer.
    ///
    /// This does not check the kind, because the caller that has a reason to build one of these has
    /// already asked [`HeapKind::of`]. Checking twice would be paying twice for the same answer on
    /// the path every call goes through.
    #[must_use]
    pub fn from_slot(slot: Slot) -> Option<ClosureRef> {
        slot.is_pointer().then_some(ClosureRef(slot))
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

/// Bytes in a native: the kind tag and the ordinal.
///
/// Eight exactly, which is the smallest an object in this heap can be, and there is nothing else to
/// put in it. A native has no captured environment because Rust code closes over nothing, and no
/// name here because the name is in the same table the code is in.
const NATIVE_SIZE: usize = 8;
/// Which entry in the isolate's table of Rust functions this native calls.
const ORDINAL_OFFSET: usize = 4;

/// A function whose body is Rust: an ordinal into the table of them the isolate holds.
///
/// The code pointer is not in here, and that is the whole design of this object. A function pointer
/// is eight bytes and everything in the cage that refers to anything is four, so putting one in
/// would either widen every reference or need its own encoding. It is also a pointer into the text
/// segment sitting in a data heap, which every collector, every snapshot and every AOT image would
/// then have to know about and fix up. An ordinal has none of those properties: it is a small
/// integer that means the same thing in any process that has the same table, and looking a function
/// up by it is one bounds checked index on a path that is about to make a Rust call anyway.
///
/// The name is in the table for the same reason. A closure keeps its own name because the unit that
/// compiled it can be dropped while the value lives on, so there would be nothing left to ask. A
/// native cannot outlive its table: the table is in the isolate that owns the cage the native is
/// allocated in, so anything holding this reference can reach the name already.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativeRef(Slot);

impl std::fmt::Debug for NativeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native@{:?}", self.0)
    }
}

impl NativeRef {
    /// Stamp out a native calling entry `ordinal` of the isolate's table.
    ///
    /// Returns `None` if the heap is full.
    #[must_use]
    pub fn new(heap: &mut BumpHeap, ordinal: u32) -> Option<NativeRef> {
        let pointer = heap.allocate(NATIVE_SIZE, ObjectKind::Native)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `NATIVE_SIZE` bytes of freshly committed memory that nothing else
        // holds a reference to, and both writes are inside it.
        unsafe {
            write_kind(pointer, HeapKind::Native);
            write_u32(pointer, ORDINAL_OFFSET, ordinal);
        }
        Some(NativeRef(slot))
    }

    /// Which entry in the table this native calls.
    #[must_use]
    pub fn ordinal(self, cage: &Cage) -> u32 {
        // SAFETY: the slot points at a native, which is `NATIVE_SIZE` bytes long.
        unsafe { read_u32(cage, self.offset(), ORDINAL_OFFSET) }
    }

    /// The slot this native lives at.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read a native back out of a slot, or `None` if the slot is not a pointer.
    ///
    /// As with [`ClosureRef::from_slot`], the kind is not rechecked here.
    #[must_use]
    pub fn from_slot(slot: Slot) -> Option<NativeRef> {
        slot.is_pointer().then_some(NativeRef(slot))
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

/// Bytes of header on a context, before the cells.
///
/// Three words of bookkeeping and a fourth of padding, because a cell is eight bytes and has to
/// start on an eight byte boundary. The padding is not waste that could be recovered by rearranging:
/// there is nothing else to put there until a context needs a shape.
const CONTEXT_HEADER_SIZE: usize = 16;
/// The enclosing context, as raw slot bits, or zero for none.
const PARENT_OFFSET: usize = 4;
/// How many cells this context holds.
const LENGTH_OFFSET: usize = 8;
/// Bytes per cell.
const CELL_SIZE: usize = 8;

/// One level of environment: the variables at this level, and the level outside it.
///
/// A context exists only when scope analysis found a variable a nested function reads. That is why
/// `cell_slots` on a blueprint is usually zero and why the chain a closure walks is shorter than the
/// nesting of the source, which is the thing that makes `hops` a small number in practice.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextRef(Slot);

impl std::fmt::Debug for ContextRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "env@{:?}", self.0)
    }
}

impl ContextRef {
    /// Allocate a context with `cells` cells, nested inside `parent`.
    ///
    /// Every cell starts as the raw bits given by `empty`, which the caller passes because the value
    /// encoding lives a layer up from the heap. Zeroed memory would be a valid double rather than a
    /// hole, and a cell that reads as zero when the program has not run its declaration yet is the
    /// dead zone silently answering wrong.
    ///
    /// Returns `None` if the heap is full or the context is larger than the cage.
    #[must_use]
    pub fn new(
        heap: &mut BumpHeap,
        parent: Option<ContextRef>,
        cells: u32,
        empty: u64,
    ) -> Option<ContextRef> {
        let bytes = CONTEXT_HEADER_SIZE.checked_add((cells as usize).checked_mul(CELL_SIZE)?)?;
        let pointer = heap.allocate(bytes, ObjectKind::Context)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `bytes` long and every write below is inside it.
        unsafe {
            write_kind(pointer, HeapKind::Context);
            write_u32(
                pointer,
                PARENT_OFFSET,
                parent.map_or(0, |context| context.0.to_bits()),
            );
            write_u32(pointer, LENGTH_OFFSET, cells);
            for index in 0..cells as usize {
                write_u64(
                    pointer.as_ptr(),
                    CONTEXT_HEADER_SIZE + index * CELL_SIZE,
                    empty,
                );
            }
        }
        Some(ContextRef(slot))
    }

    /// The context this one is nested inside, or `None` at the outermost level.
    #[must_use]
    pub fn parent(self, cage: &Cage) -> Option<ContextRef> {
        // SAFETY: the slot points at a context, whose header is `CONTEXT_HEADER_SIZE` bytes.
        let bits = unsafe { read_u32(cage, self.offset(), PARENT_OFFSET) };
        ContextRef::from_slot(Slot::from_bits(bits))
    }

    /// How many cells this context holds.
    #[must_use]
    pub fn len(self, cage: &Cage) -> u32 {
        // SAFETY: as `parent`.
        unsafe { read_u32(cage, self.offset(), LENGTH_OFFSET) }
    }

    /// Whether this context holds no cells at all, which lowering does not currently produce.
    #[must_use]
    pub fn is_empty(self, cage: &Cage) -> bool {
        self.len(cage) == 0
    }

    /// Read a cell, or `None` if the index is past the end.
    ///
    /// Bounds checked rather than trusted, because the index comes from bytecode and a wrong one
    /// would read the object that happens to sit after this context in the heap.
    #[must_use]
    pub fn cell(self, cage: &Cage, index: u32) -> Option<u64> {
        if index >= self.len(cage) {
            return None;
        }
        let at = CONTEXT_HEADER_SIZE + (index as usize) * CELL_SIZE;
        // SAFETY: the index is inside the length the header records, so the cell is inside the
        // allocation, and it was initialised when the context was created.
        Some(unsafe { read_u64(cage.address_of(self.offset()), at) })
    }

    /// Write a cell, doing nothing and answering `false` if the index is past the end.
    ///
    /// Takes the heap rather than the cage because this is a mutation, and the heap is the thing a
    /// caller has to hold exclusively to be allowed to make one.
    pub fn set_cell(self, heap: &mut BumpHeap, index: u32, bits: u64) -> bool {
        if index >= self.len(heap.cage()) {
            return false;
        }
        let at = CONTEXT_HEADER_SIZE + (index as usize) * CELL_SIZE;
        // SAFETY: as `cell`, and `&mut BumpHeap` means nothing else is reading the cage.
        unsafe {
            write_u64(heap.cage().address_of(self.offset()), at, bits);
        }
        true
    }

    /// The slot this context lives at.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read a context back out of a slot, or `None` if the slot is not a pointer.
    ///
    /// Zero is unambiguous as an absence, which is what a parent field holds at the outermost level
    /// and what a frame holds when its function captured nothing. A slot holding a pointer always
    /// has its low bit set, so the smallest pointer slot is one and zero is the small integer zero,
    /// which nothing ever stores in a parent field.
    ///
    /// As with [`ClosureRef::from_slot`], the kind is not rechecked here.
    #[must_use]
    pub fn from_slot(slot: Slot) -> Option<ContextRef> {
        slot.is_pointer().then_some(ContextRef(slot))
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClosureRef, ContextRef, HeapKind, NativeRef};
    use crate::bump::BumpHeap;
    use crate::string::StringRef;

    fn heap() -> BumpHeap {
        BumpHeap::new().expect("should reserve a cage")
    }

    /// The hole, spelled the way the value encoding spells it, so the tests do not depend on it.
    const EMPTY: u64 = 0;

    #[test]
    fn a_string_still_reads_as_a_string_without_anybody_writing_a_tag() {
        // The whole reason zero means string. Nothing in the string allocator was changed to make
        // this pass, and if that ever stops being true this test is where it shows up.
        let mut heap = heap();
        let string = StringRef::from_str(&mut heap, "katsu").expect("should have room");
        assert_eq!(
            HeapKind::of(heap.cage(), string.slot()),
            Some(HeapKind::String)
        );
    }

    #[test]
    fn the_four_kinds_are_told_apart() {
        let mut heap = heap();
        let string = StringRef::from_str(&mut heap, "katsu").expect("should have room");
        let context = ContextRef::new(&mut heap, None, 2, EMPTY).expect("should have room");
        let closure = ClosureRef::new(&mut heap, 7, Some(context), None).expect("should have room");
        let native = NativeRef::new(&mut heap, 3).expect("should have room");
        assert_eq!(
            HeapKind::of(heap.cage(), string.slot()),
            Some(HeapKind::String)
        );
        assert_eq!(
            HeapKind::of(heap.cage(), context.slot()),
            Some(HeapKind::Context)
        );
        assert_eq!(
            HeapKind::of(heap.cage(), closure.slot()),
            Some(HeapKind::Closure)
        );
        assert_eq!(
            HeapKind::of(heap.cage(), native.slot()),
            Some(HeapKind::Native)
        );
    }

    #[test]
    fn a_native_remembers_which_entry_it_calls() {
        let mut heap = heap();
        let first = NativeRef::new(&mut heap, 0).expect("should have room");
        let later = NativeRef::new(&mut heap, 41).expect("should have room");
        assert_eq!(first.ordinal(heap.cage()), 0);
        assert_eq!(later.ordinal(heap.cage()), 41);
    }

    #[test]
    fn a_native_costs_eight_bytes_and_wastes_none_of_them() {
        // The smallest object this heap can hand out, and the reason the name and the code pointer
        // are both in the table rather than in here. If this ever needs a third word, the tradeoff
        // in the doc above has to be made again rather than assumed.
        use crate::bump::ObjectKind;
        let mut heap = heap();
        NativeRef::new(&mut heap, 0).expect("should have room");
        let totals = heap.census().totals(ObjectKind::Native);
        assert_eq!(totals.count, 1);
        assert_eq!(totals.requested_bytes, 8);
        assert_eq!(totals.reserved_bytes, 8);
    }

    #[test]
    fn a_small_integer_is_not_a_pointer_and_has_no_kind() {
        let heap = heap();
        let slot = crate::cage::Slot::from_smi(41).expect("fits");
        assert_eq!(HeapKind::of(heap.cage(), slot), None);
    }

    #[test]
    fn a_closure_carries_its_own_name_so_it_can_be_printed_without_its_unit() {
        let mut heap = heap();
        let name = StringRef::from_str(&mut heap, "greet").expect("should have room");
        let named = ClosureRef::new(&mut heap, 0, None, Some(name)).expect("should have room");
        let anonymous = ClosureRef::new(&mut heap, 0, None, None).expect("should have room");
        assert_eq!(named.name(heap.cage()), Some(name));
        assert_eq!(anonymous.name(heap.cage()), None);
    }

    #[test]
    fn a_closure_remembers_its_function_and_what_it_captured() {
        let mut heap = heap();
        let context = ContextRef::new(&mut heap, None, 1, EMPTY).expect("should have room");
        let closure = ClosureRef::new(&mut heap, 3, Some(context), None).expect("should have room");
        assert_eq!(closure.function(heap.cage()), 3);
        assert_eq!(closure.captured(heap.cage()), Some(context));
    }

    #[test]
    fn a_closure_over_nothing_says_so_rather_than_pointing_at_the_bottom_of_the_cage() {
        // Zero in the captured field has to mean nothing captured, not the object at offset zero.
        // A pointer slot always has its low bit set, so the two can never collide, and this is the
        // test that says so.
        let mut heap = heap();
        let first = ContextRef::new(&mut heap, None, 1, EMPTY).expect("should have room");
        assert_ne!(first.slot().to_bits(), 0);
        let closure = ClosureRef::new(&mut heap, 0, None, None).expect("should have room");
        assert_eq!(closure.captured(heap.cage()), None);
    }

    #[test]
    fn a_cell_holds_what_was_written_into_it() {
        let mut heap = heap();
        let context = ContextRef::new(&mut heap, None, 3, EMPTY).expect("should have room");
        assert_eq!(context.len(heap.cage()), 3);
        assert!(context.set_cell(&mut heap, 1, 0x4000_0000_0000_0001));
        assert_eq!(context.cell(heap.cage(), 0), Some(EMPTY));
        assert_eq!(context.cell(heap.cage(), 1), Some(0x4000_0000_0000_0001));
        assert_eq!(context.cell(heap.cage(), 2), Some(EMPTY));
    }

    #[test]
    fn every_cell_starts_as_the_hole_rather_than_as_zero() {
        // Zeroed memory is a perfectly good double, so a context that left its cells at zero would
        // answer a dead zone check with the number zero instead of with the hole.
        let mut heap = heap();
        let context = ContextRef::new(&mut heap, None, 4, 0xDEAD_BEEF).expect("should have room");
        for index in 0..4 {
            assert_eq!(context.cell(heap.cage(), index), Some(0xDEAD_BEEF));
        }
    }

    #[test]
    fn a_cell_past_the_end_is_refused_rather_than_read_out_of_the_next_object() {
        let mut heap = heap();
        let context = ContextRef::new(&mut heap, None, 2, EMPTY).expect("should have room");
        let after = ContextRef::new(&mut heap, None, 2, 0x1111).expect("should have room");
        assert_eq!(context.cell(heap.cage(), 2), None);
        assert!(!context.set_cell(&mut heap, 9, 0x2222));
        assert_eq!(after.cell(heap.cage(), 0), Some(0x1111));
    }

    #[test]
    fn the_chain_of_parents_walks_outwards() {
        let mut heap = heap();
        let outer = ContextRef::new(&mut heap, None, 1, EMPTY).expect("should have room");
        let middle = ContextRef::new(&mut heap, Some(outer), 1, EMPTY).expect("should have room");
        let inner = ContextRef::new(&mut heap, Some(middle), 1, EMPTY).expect("should have room");
        assert_eq!(inner.parent(heap.cage()), Some(middle));
        assert_eq!(middle.parent(heap.cage()), Some(outer));
        assert_eq!(outer.parent(heap.cage()), None);
    }

    #[test]
    fn contexts_and_closures_land_in_their_own_census_lines() {
        // The census exists so that the memory budget can be read line by line, and a new kind of
        // object that counted itself as something else would make that reading wrong.
        use crate::bump::ObjectKind;
        let mut heap = heap();
        ContextRef::new(&mut heap, None, 1, EMPTY).expect("should have room");
        ClosureRef::new(&mut heap, 0, None, None).expect("should have room");
        assert_eq!(heap.census().totals(ObjectKind::Closure).count, 1);
        assert_eq!(heap.census().totals(ObjectKind::Context).count, 1);
        assert_eq!(heap.census().totals(ObjectKind::String).count, 0);
        // Twenty four bytes for the context, which is sixteen of header and one eight byte cell,
        // and sixteen for the closure with no alignment tax on either.
        assert_eq!(heap.census().totals(ObjectKind::Context).reserved_bytes, 24);
        assert_eq!(
            heap.census().totals(ObjectKind::Closure).requested_bytes,
            16
        );
        assert_eq!(heap.census().totals(ObjectKind::Closure).reserved_bytes, 16);
    }
}
