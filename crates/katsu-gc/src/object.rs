//! What every object in the cage has in common, which today is one word and a way to read it.
//!
//! Until the interpreter could call a function, every pointer in the cage was a string, and one
//! place said so out loud so that the day it stopped being true there was one thing to change
//! rather than twenty. There are nine kinds now and there will be more, so the word they share and
//! the raw reads and writes that reach it live here rather than in the file that happened to add
//! the second kind.
//!
//! # How the kinds are told apart
//!
//! Every object in the cage starts with the same word, which `spec/07-object-model.md` calls the
//! shape reference and which is where the map pointer goes when maps exist in M1. It is a [`Slot`],
//! so its low bit says whether it holds a pointer or a small integer. A shape will be a pointer. A
//! kind tag is a small integer. So a kind tag written there today is not something a shape can ever
//! be mistaken for, and when shapes arrive the tag does not have to move: the string's shape carries
//! the same answer the tag does, and the check becomes a read through the map instead of a compare
//! against a constant.
//!
//! Zero is a string, and that is not an arbitrary assignment. A freshly committed page is zero and
//! the bump heap never reuses memory, so every string ever allocated already has a zero in that word
//! without a single instruction being spent on it. Adding a kind tag to strings would have cost a
//! store per string allocation to encode the thing the memory already said.

use std::ptr::NonNull;

use crate::cage::{Cage, Slot};

/// The word every object in the cage starts with, holding a shape in M1 and a kind tag today.
pub(crate) const KIND_OFFSET: usize = 0;

/// What kind of object a pointer in the cage points at.
///
/// The enum is deliberately not exhaustive over what the rest of M1 adds, because the point of it is
/// to answer the question the interpreter actually asks: is this a string, is it callable, is it
/// something with properties on it, or is it something the caller has no arm for yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapKind {
    /// A string in any of its representations.
    String,
    /// A closure: a blueprint plus the environment it captured.
    Closure,
    /// One level of environment, holding the variables a nested function captured.
    Context,
    /// A function whose body is Rust rather than bytecode.
    Native,
    /// An ordinary object: the thing a program makes with `{}` and puts properties on.
    ///
    /// The only kind with no tag of its own, because its first word holds a shape and a shape is a
    /// pointer. That is the arrangement the whole file was written to allow, and it means the check
    /// for "is this an object" is a tag test on one word rather than a compare against a constant.
    Object,
    /// A shape, which is shared by every object with the same properties in the same order.
    Shape,
    /// The values of an object that did not fit in the room it was built with.
    Properties,
    /// A getter and a setter, sitting in the slot where an ordinary property keeps its value.
    AccessorPair,
    /// The indexed properties of an object, addressed by the integer rather than by a name.
    Elements,
}

impl HeapKind {
    /// The tag this kind writes into the first word, for the kinds that write one.
    ///
    /// A small integer rather than a raw number, so the low bit says integer rather than pointer and
    /// a shape can never be read as a kind by accident. An ordinary object has no tag: it writes a
    /// shape into that word instead, which is what tells it apart from all of these.
    pub(crate) const fn tag(self) -> Option<u32> {
        let n: u32 = match self {
            HeapKind::String => 0,
            HeapKind::Closure => 1,
            HeapKind::Context => 2,
            HeapKind::Native => 3,
            HeapKind::Shape => 4,
            HeapKind::Properties => 5,
            HeapKind::AccessorPair => 6,
            HeapKind::Elements => 7,
            HeapKind::Object => return None,
        };
        // `from_smi` cannot fail for a number this small, and `expect` is not const, so the shift is
        // spelled out. It is the same shift `Slot::from_smi` performs.
        Some(n << 1)
    }

    /// What a slot points at, or `None` if it is not a pointer or points at a tag nobody wrote.
    ///
    /// An unrecognised tag is `None` rather than a panic. Reaching one means a value was fabricated
    /// or the heap was corrupted, and neither is worth taking the process down for when the caller
    /// has a perfectly good "this is not the kind you wanted" path already.
    #[must_use]
    pub fn of(cage: &Cage, slot: Slot) -> Option<HeapKind> {
        let offset = slot.as_offset()?;
        // SAFETY: the offset came out of a slot that was built from a cage offset, and every
        // allocated object is at least one word long, so the first word is inside the cage.
        let first = Slot::from_bits(unsafe { read_u32(cage, offset, KIND_OFFSET) });
        // A pointer in that word is a shape, and only an ordinary object has one there. The pointee
        // is not checked, because the alternative to trusting it is a second dereference on the
        // hottest path in the runtime to re-derive something only a corrupted heap could disagree
        // with.
        if first.is_pointer() {
            return Some(HeapKind::Object);
        }
        match first.to_bits() {
            0 => Some(HeapKind::String),
            2 => Some(HeapKind::Closure),
            4 => Some(HeapKind::Context),
            6 => Some(HeapKind::Native),
            8 => Some(HeapKind::Shape),
            10 => Some(HeapKind::Properties),
            12 => Some(HeapKind::AccessorPair),
            14 => Some(HeapKind::Elements),
            _ => None,
        }
    }
}

/// The offset a freshly allocated pointer sits at, as a slot.
pub(crate) fn slot_of(cage: &Cage, pointer: NonNull<u8>) -> Option<Slot> {
    cage.offset_of(pointer.as_ptr()).map(Slot::from_offset)
}

/// Stamp the kind tag into the first word of a freshly allocated object.
///
/// One place holds the knowledge that an ordinary object is the kind without a tag, so that every
/// other kind writes its own with a single call and nobody has to think about it.
///
/// # Panics
///
/// Panics if `kind` is [`HeapKind::Object`], which writes a shape into that word rather than a tag.
///
/// # Safety
///
/// `pointer` must be the start of an allocation at least four bytes long.
pub(crate) unsafe fn write_kind(pointer: NonNull<u8>, kind: HeapKind) {
    let tag = kind
        .tag()
        .expect("an ordinary object writes its shape into the first word rather than a tag");
    // SAFETY: the caller guarantees the allocation is long enough for its first word.
    unsafe {
        write_u32(pointer, KIND_OFFSET, tag);
    }
}

/// Write a four byte field of an object that is already in the cage.
///
/// The companion to [`write_u32`], which takes the pointer an allocation just handed back. This one
/// starts from an offset, which is what a reference into the heap actually holds.
///
/// # Safety
///
/// `offset` must be the start of an allocation in `cage` at least `at + 4` bytes long, and nothing
/// else may be reading the cage for the duration of the write.
pub(crate) unsafe fn write_u32_at(cage: &Cage, offset: u32, at: usize, value: u32) {
    // SAFETY: as `write_u32`, with the address derived from the cage rather than passed in.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        cage.address_of(offset).add(at).cast::<u32>().write(value);
    }
}

/// # Safety
///
/// `pointer` must be the start of an allocation at least `at + 4` bytes long.
pub(crate) unsafe fn write_u32(pointer: NonNull<u8>, at: usize, value: u32) {
    // SAFETY: the caller guarantees the write is inside the allocation. Every offset this crate
    // passes is a header word or a four byte field in an array of them, and both are four byte
    // aligned inside an object the heap aligned to eight, which is what clippy cannot see through a
    // byte pointer.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        pointer.as_ptr().add(at).cast::<u32>().write(value);
    }
}

/// # Safety
///
/// `offset` must be the start of an allocation in `cage` at least `at + 4` bytes long.
pub(crate) unsafe fn read_u32(cage: &Cage, offset: u32, at: usize) -> u32 {
    // SAFETY: as `write_u32`, and the header is written before a reference to the object escapes,
    // so there is no window in which this reads uninitialised memory.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        cage.address_of(offset).add(at).cast::<u32>().read()
    }
}

/// # Safety
///
/// `base` must be the start of an allocation at least `at + 8` bytes long.
pub(crate) unsafe fn write_u64(base: *mut u8, at: usize, value: u64) {
    // SAFETY: the caller guarantees the write is inside the allocation. The eight byte fields in
    // this crate are context cells and record values, and both start at an eight byte aligned
    // offset inside an eight byte aligned object.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        base.add(at).cast::<u64>().write(value);
    }
}

/// # Safety
///
/// `base` must be the start of an allocation at least `at + 8` bytes long, with that field written.
pub(crate) unsafe fn read_u64(base: *mut u8, at: usize) -> u64 {
    // SAFETY: as `write_u64`, and every one of those fields is written when its object is created.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        base.add(at).cast::<u64>().read()
    }
}
