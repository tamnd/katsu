//! An ordinary object: a shape that says which properties it has, and the values behind them.
//!
//! This is what a program makes with `{}` and what every host object in the runtime is. It replaces
//! the record, which was a fixed set of names decided when it was built, and whose own
//! documentation said that the day shapes arrived it would either grow one or stop existing. It
//! stopped existing.
//!
//! # The layout
//!
//! ```text
//! [shape][properties][inline capacity][padding][inline slot 0][inline slot 1]...
//! ```
//!
//! The shape is a pointer, and it is in the first word every object in the cage has, which is what
//! tells an ordinary object apart from a string or a closure without a tag of its own. See
//! `object.rs`.
//!
//! The values go in the object itself for as long as there is room, which is the arrangement worth
//! having: reading a property of an object built from a literal is one load from the object rather
//! than a load of a pointer and then a load through it. An object that grows past the room it was
//! built with puts the rest in a properties array off to the side and keeps the ones that already
//! fit where they are, so `o.x = 1` on an object that had room stays one store.
//!
//! `spec/07-object-model.md` writes the layout with an elements pointer between the shape and the
//! properties, for indexed properties. There are no arrays yet, so that word is not here yet: an
//! object carrying a word nothing reads is four bytes per object against a memory budget that is the
//! point of the project. It arrives with elements, and it moves the two offsets after it.
//!
//! # Why the inline capacity is in the object and not in the shape
//!
//! It is the thing V8 keeps in the map, as the instance size, and keeping it there would save four
//! bytes per object. It cannot go there yet, because the same shape is reached both by a literal
//! that was built with room for its properties and by an empty object that had them added one at a
//! time, and those two have different amounts of room. Making the shape carry the size means shapes
//! stop being shared between those two paths, which costs more than the word saves until there is
//! the slack tracking that makes V8's version work.
//!
//! # What is not here
//!
//! No prototype, so a property that is missing is missing rather than looked for further up. No
//! attributes, so nothing is read only, non enumerable or an accessor. No delete, which is the one
//! operation a transition tree genuinely does not want and which needs the dictionary mode that
//! every engine falls back to. No indexed properties. Each of those is its own piece of work and
//! each of them is in M1.

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};
use crate::object::{
    HeapKind, read_u32, read_u64, slot_of, write_kind, write_u32, write_u32_at, write_u64,
};
use crate::shape::ShapeRef;
use crate::string::StringRef;

/// Where the shape goes, which is the word every object in the cage starts with.
const SHAPE_OFFSET: usize = 0;
/// The overflow array, or a small integer zero when everything fits inside the object.
const PROPERTIES_OFFSET: usize = 4;
/// How many values fit inside the object.
const INLINE_OFFSET: usize = 8;
/// Bytes before the first inline value. Twelve of header rounded up, because a value is eight bytes
/// and an unaligned one costs more on every read than the four bytes of padding cost once.
const HEADER_SIZE: usize = 16;
/// Bytes per value, which is a whole boxed value rather than a compressed slot.
///
/// Four would be the point of pointer compression and it is not possible yet, for the reason
/// contexts give: there is nowhere to put a double or an `undefined` until there are heap numbers
/// and realm singletons to point at. The size is one constant so that the day those exist this is
/// one edit and a set of tests that already say what the answer should be.
const VALUE_SIZE: usize = 8;

/// Bytes of header on the overflow array: the kind tag and the capacity.
const PROPERTIES_HEADER: usize = 8;
/// How many values the overflow array has room for.
const CAPACITY_OFFSET: usize = 4;
/// How many values the first overflow array holds.
///
/// Four rather than one, because an object that has outgrown its inline room is an object being
/// built by assignment, and those add more than one property almost every time.
const FIRST_OVERFLOW: u32 = 4;

/// An object with properties on it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectRef(Slot);

impl std::fmt::Debug for ObjectRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "object@{:?}", self.0)
    }
}

impl ObjectRef {
    /// Build an object with `shape`, with room inside it for `inline` values.
    ///
    /// The shape is normally the root, and `inline` is how many properties the caller is about to
    /// add: an object literal knows its property count before it builds anything, which is the
    /// whole reason the count is a parameter rather than a guess. An object built with a shape that
    /// already has properties has room for them but no values in it, so the caller has to write them
    /// before anything reads them.
    ///
    /// Returns `None` if the heap is full or the object would be larger than the cage.
    #[must_use]
    pub fn new(heap: &mut BumpHeap, shape: ShapeRef, inline: u32) -> Option<ObjectRef> {
        let bytes = HEADER_SIZE.checked_add((inline as usize).checked_mul(VALUE_SIZE)?)?;
        let pointer = heap.allocate(bytes, ObjectKind::Object)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `bytes` long, which covers the header. The properties word is
        // left as the zero the heap hands out, which reads back as "there is no overflow array".
        unsafe {
            write_u32(pointer, SHAPE_OFFSET, shape.slot().to_bits());
            write_u32(pointer, INLINE_OFFSET, inline);
        }
        Some(ObjectRef(slot))
    }

    /// This object's shape, which is what says where its properties are.
    #[must_use]
    pub fn shape(self, cage: &Cage) -> ShapeRef {
        ShapeRef::from_slot(Slot::from_bits(self.field(cage, SHAPE_OFFSET)))
            .expect("an ordinary object has a shape in its first word, which is what makes it one")
    }

    /// How many properties this object has.
    #[must_use]
    pub fn len(self, cage: &Cage) -> u32 {
        self.shape(cage).count(cage)
    }

    /// Whether this object has no properties at all.
    #[must_use]
    pub fn is_empty(self, cage: &Cage) -> bool {
        self.len(cage) == 0
    }

    /// The value stored under `name`, or `None` if this object has no such property.
    ///
    /// There is no prototype chain yet, so a miss here is the final answer rather than the start of
    /// a walk.
    #[must_use]
    pub fn get(self, cage: &Cage, name: StringRef) -> Option<u64> {
        let index = self.shape(cage).index_of(cage, name)?;
        self.value_at(cage, index)
    }

    /// Store `value` under `name`, adding the property if the object does not have it.
    ///
    /// This is where an object grows a shape. Adding a property takes a transition, and taking the
    /// same transition from the same shape twice reaches the same node, so the millionth object
    /// built like the first shares its layout.
    ///
    /// Both allocations happen before anything about the object changes, so an object whose heap ran
    /// out is the object it was rather than one with a shape claiming a property it has no room for.
    ///
    /// Returns `None` if the heap is full.
    #[must_use]
    pub fn set(self, heap: &mut BumpHeap, name: StringRef, value: u64) -> Option<()> {
        let shape = self.shape(heap.cage());
        if let Some(index) = shape.index_of(heap.cage(), name) {
            self.write(heap, index, value);
            return Some(());
        }
        let index = shape.count(heap.cage());
        let grown = shape.transition(heap, name)?;
        self.reserve(heap, index)?;
        // SAFETY: the object is in the cage and its first word is inside it, and `&mut BumpHeap`
        // means nothing else is reading the cage.
        unsafe {
            write_u32_at(
                heap.cage(),
                self.offset(),
                SHAPE_OFFSET,
                grown.slot().to_bits(),
            );
        }
        self.write(heap, index, value);
        Some(())
    }

    /// Every name on this object, in the order the properties were added.
    ///
    /// Together with [`ObjectRef::value_at`] this is how printing walks an object and how the
    /// collector will, both of which want insertion order rather than a name to look up. It is the
    /// whole list rather than one name at a time because the names are a chain, so asking for the
    /// nth on its own would walk the chain n times to print an object once.
    #[must_use]
    pub fn names(self, cage: &Cage) -> Vec<StringRef> {
        self.shape(cage).names(cage)
    }

    /// The value of the property at `index`, or `None` if the index is past the end.
    #[must_use]
    pub fn value_at(self, cage: &Cage, index: u32) -> Option<u64> {
        if index >= self.len(cage) {
            return None;
        }
        let inline = self.inline(cage);
        if index < inline {
            // SAFETY: the index is below the inline capacity the header records, so the value is
            // inside the allocation, and it is below the property count, so it has been written.
            return Some(unsafe {
                read_u64(
                    cage.address_of(self.offset()),
                    HEADER_SIZE + (index as usize) * VALUE_SIZE,
                )
            });
        }
        let properties = self.properties(cage)?;
        let at = index - inline;
        if at >= properties.capacity(cage) {
            return None;
        }
        // SAFETY: the index is below the capacity the array's own header records.
        Some(unsafe {
            read_u64(
                cage.address_of(properties.offset()),
                PROPERTIES_HEADER + (at as usize) * VALUE_SIZE,
            )
        })
    }

    /// The slot this object lives at.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read an object back out of a slot, or `None` if the slot is not a pointer.
    ///
    /// As with the function objects, the kind is not rechecked here, because the caller reached this
    /// through [`HeapKind::of`] and checking twice pays twice for one answer.
    #[must_use]
    pub const fn from_slot(slot: Slot) -> Option<ObjectRef> {
        if slot.is_pointer() {
            Some(ObjectRef(slot))
        } else {
            None
        }
    }

    /// How many values fit inside the object itself.
    fn inline(self, cage: &Cage) -> u32 {
        self.field(cage, INLINE_OFFSET)
    }

    /// The overflow array, if this object has outgrown the room it was built with.
    fn properties(self, cage: &Cage) -> Option<Properties> {
        Properties::from_slot(Slot::from_bits(self.field(cage, PROPERTIES_OFFSET)))
    }

    /// Make sure there is somewhere to put the value of the property at `index`.
    ///
    /// Nothing to do while the object still has inline room. Past that it needs an overflow array
    /// big enough, which means allocating the first one or replacing a full one with a larger copy.
    /// Doubling rather than growing by one, because an object that has added one property by
    /// assignment is an object that is about to add another.
    fn reserve(self, heap: &mut BumpHeap, index: u32) -> Option<()> {
        let inline = self.inline(heap.cage());
        if index < inline {
            return Some(());
        }
        let wanted = index - inline + 1;
        let existing = self.properties(heap.cage());
        let capacity = existing.map_or(0, |array| array.capacity(heap.cage()));
        if wanted <= capacity {
            return Some(());
        }
        let grown = capacity.checked_mul(2)?.max(FIRST_OVERFLOW).max(wanted);
        let array = Properties::new(heap, grown)?;
        for slot in 0..capacity {
            let value = existing
                .expect("a capacity above zero means there is an array to read it from")
                .value_at(heap.cage(), slot);
            array.set(heap, slot, value);
        }
        // SAFETY: the object is in the cage and its properties word is inside it.
        unsafe {
            write_u32_at(
                heap.cage(),
                self.offset(),
                PROPERTIES_OFFSET,
                array.slot().to_bits(),
            );
        }
        Some(())
    }

    /// Write the value of the property at `index`, which [`ObjectRef::reserve`] has made room for.
    fn write(self, heap: &mut BumpHeap, index: u32, value: u64) {
        let inline = self.inline(heap.cage());
        if index < inline {
            // SAFETY: the index is below the inline capacity, so the write is inside the object,
            // and `&mut BumpHeap` means nothing else is reading the cage.
            unsafe {
                write_u64(
                    heap.cage().address_of(self.offset()),
                    HEADER_SIZE + (index as usize) * VALUE_SIZE,
                    value,
                );
            }
            return;
        }
        let array = self
            .properties(heap.cage())
            .expect("reserve puts an array there before anything is written past the inline slots");
        array.set(heap, index - inline, value);
    }

    /// One of the three header words, as raw bits.
    fn field(self, cage: &Cage, at: usize) -> u32 {
        // SAFETY: the slot points at an object, and every offset this is called with is one of its
        // own header words, all of which are written before the object escapes.
        unsafe { read_u32(cage, self.offset(), at) }
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

/// The values of an object that did not fit in the room it was built with.
///
/// A plain array with a capacity, and not a second object model. It has no names in it: which name
/// a slot belongs to is the shape's answer, exactly as it is for the inline slots, so an object's
/// properties are one numbered sequence that happens to be stored in two places.
#[derive(Clone, Copy)]
struct Properties(Slot);

impl Properties {
    /// Allocate an array with room for `capacity` values.
    fn new(heap: &mut BumpHeap, capacity: u32) -> Option<Properties> {
        let bytes = PROPERTIES_HEADER.checked_add((capacity as usize).checked_mul(VALUE_SIZE)?)?;
        let pointer = heap.allocate(bytes, ObjectKind::Properties)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `bytes` long, which covers the header.
        unsafe {
            write_kind(pointer, HeapKind::Properties);
            write_u32(pointer, CAPACITY_OFFSET, capacity);
        }
        Some(Properties(slot))
    }

    /// How many values this array has room for.
    fn capacity(self, cage: &Cage) -> u32 {
        // SAFETY: the slot points at a properties array, whose header is `PROPERTIES_HEADER` bytes.
        unsafe { read_u32(cage, self.offset(), CAPACITY_OFFSET) }
    }

    /// The value at `index`, which the caller has checked against the capacity.
    fn value_at(self, cage: &Cage, index: u32) -> u64 {
        // SAFETY: the caller checked the index against the capacity in the header.
        unsafe {
            read_u64(
                cage.address_of(self.offset()),
                PROPERTIES_HEADER + (index as usize) * VALUE_SIZE,
            )
        }
    }

    /// Write the value at `index`, which the caller has checked against the capacity.
    fn set(self, heap: &mut BumpHeap, index: u32, value: u64) {
        // SAFETY: as `value_at`, and `&mut BumpHeap` means nothing else is reading the cage.
        unsafe {
            write_u64(
                heap.cage().address_of(self.offset()),
                PROPERTIES_HEADER + (index as usize) * VALUE_SIZE,
                value,
            );
        }
    }

    fn slot(self) -> Slot {
        self.0
    }

    fn from_slot(slot: Slot) -> Option<Properties> {
        slot.is_pointer().then_some(Properties(slot))
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{FIRST_OVERFLOW, HEADER_SIZE, ObjectRef, PROPERTIES_HEADER, VALUE_SIZE};
    use crate::bump::{BumpHeap, ObjectKind};
    use crate::cage::Slot;
    use crate::object::HeapKind;
    use crate::shape::ShapeRef;
    use crate::string::StringRef;

    fn heap() -> BumpHeap {
        BumpHeap::new().expect("should reserve a cage")
    }

    fn name(heap: &mut BumpHeap, text: &str) -> StringRef {
        StringRef::from_str(heap, text).expect("should have room")
    }

    fn empty(heap: &mut BumpHeap, inline: u32) -> ObjectRef {
        let root = ShapeRef::root(heap).expect("should have room");
        ObjectRef::new(heap, root, inline).expect("should have room")
    }

    fn texts(heap: &BumpHeap, object: ObjectRef) -> Vec<String> {
        object
            .names(heap.cage())
            .into_iter()
            .map(|name| name.to_utf8_lossy(heap.cage()).into_owned())
            .collect()
    }

    #[test]
    fn an_object_is_told_apart_by_having_a_shape_where_a_tag_would_be() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        assert_eq!(
            HeapKind::of(heap.cage(), object.slot()),
            Some(HeapKind::Object)
        );
        assert!(object.is_empty(heap.cage()));
    }

    #[test]
    fn a_property_that_was_put_in_comes_back_out() {
        let mut heap = heap();
        let object = empty(&mut heap, 2);
        let x = name(&mut heap, "x");
        let y = name(&mut heap, "y");
        object.set(&mut heap, x, 11).expect("should have room");
        object.set(&mut heap, y, 22).expect("should have room");
        assert_eq!(object.get(heap.cage(), x), Some(11));
        assert_eq!(object.get(heap.cage(), y), Some(22));
        assert_eq!(object.len(heap.cage()), 2);
    }

    #[test]
    fn a_property_that_was_never_put_in_is_absent_rather_than_wrong() {
        let mut heap = heap();
        let object = empty(&mut heap, 1);
        let x = name(&mut heap, "x");
        let missing = name(&mut heap, "nope");
        object.set(&mut heap, x, 11).expect("should have room");
        assert_eq!(object.get(heap.cage(), missing), None);
    }

    #[test]
    fn writing_a_property_twice_replaces_it_rather_than_adding_it() {
        let mut heap = heap();
        let object = empty(&mut heap, 1);
        let x = name(&mut heap, "x");
        object.set(&mut heap, x, 11).expect("should have room");
        let shape = object.shape(heap.cage());
        object.set(&mut heap, x, 33).expect("should have room");
        assert_eq!(object.get(heap.cage(), x), Some(33));
        assert_eq!(object.len(heap.cage()), 1);
        assert_eq!(
            object.shape(heap.cage()),
            shape,
            "an assignment to a property that exists is not a transition"
        );
    }

    #[test]
    fn an_object_grows_past_the_room_it_was_built_with() {
        // The thing a record could not do, which is the reason it stopped existing. The object is
        // built with no inline room at all, so every one of these goes to the overflow array and the
        // array itself has to grow twice on the way.
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        let names: Vec<StringRef> = (0..12)
            .map(|index| name(&mut heap, &format!("p{index}")))
            .collect();
        for (index, &name) in names.iter().enumerate() {
            object
                .set(&mut heap, name, index as u64)
                .expect("should have room");
        }
        for (index, &name) in names.iter().enumerate() {
            assert_eq!(object.get(heap.cage(), name), Some(index as u64));
        }
        assert_eq!(object.len(heap.cage()), 12);
        assert_eq!(
            heap.census().totals(ObjectKind::Properties).count,
            3,
            "room for four, then eight, then sixteen"
        );
    }

    #[test]
    fn the_properties_that_fit_inside_stay_inside_when_the_object_grows() {
        // The inline slots are not abandoned when the overflow array appears, because the point of
        // them is that reading them is one load, and copying them out would give that up for every
        // object that ever gains one property too many.
        let mut heap = heap();
        let object = empty(&mut heap, 2);
        let names: Vec<StringRef> = (0..5)
            .map(|index| name(&mut heap, &format!("p{index}")))
            .collect();
        for (index, &name) in names.iter().enumerate() {
            object
                .set(&mut heap, name, 100 + index as u64)
                .expect("should have room");
        }
        assert_eq!(
            heap.census().totals(ObjectKind::Properties).count,
            1,
            "two inline slots and one overflow array is all five properties need"
        );
        for (index, &name) in names.iter().enumerate() {
            assert_eq!(object.get(heap.cage(), name), Some(100 + index as u64));
        }
    }

    #[test]
    fn two_objects_built_the_same_way_share_one_shape() {
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        let y = name(&mut heap, "y");
        let first = ObjectRef::new(&mut heap, root, 2).expect("should have room");
        let second = ObjectRef::new(&mut heap, root, 2).expect("should have room");
        for object in [first, second] {
            object.set(&mut heap, x, 1).expect("should have room");
            object.set(&mut heap, y, 2).expect("should have room");
        }
        assert_eq!(first.shape(heap.cage()), second.shape(heap.cage()));
        assert_ne!(first.slot(), second.slot());
    }

    #[test]
    fn the_properties_can_be_walked_in_the_order_they_were_added() {
        let mut heap = heap();
        let object = empty(&mut heap, 3);
        for (index, text) in ["b", "a", "c"].into_iter().enumerate() {
            let name = name(&mut heap, text);
            object
                .set(&mut heap, name, index as u64)
                .expect("should have room");
        }
        assert_eq!(texts(&heap, object), ["b", "a", "c"]);
        assert_eq!(object.value_at(heap.cage(), 0), Some(0));
        assert_eq!(object.value_at(heap.cage(), 2), Some(2));
        assert_eq!(object.value_at(heap.cage(), 3), None);
    }

    #[test]
    fn an_object_built_with_room_for_its_properties_costs_one_allocation() {
        // What an object literal pays, and the number the memory budget is spent against: a header
        // and one eight byte value per property, with no second object anywhere.
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        let y = name(&mut heap, "y");
        let before = heap.census().totals(ObjectKind::Object);
        let object = ObjectRef::new(&mut heap, root, 2).expect("should have room");
        object.set(&mut heap, x, 1).expect("should have room");
        object.set(&mut heap, y, 2).expect("should have room");
        let after = heap.census().totals(ObjectKind::Object);
        assert_eq!(after.count - before.count, 1);
        assert_eq!(
            after.reserved_bytes - before.reserved_bytes,
            (HEADER_SIZE + 2 * VALUE_SIZE) as u64
        );
        assert_eq!(
            heap.census().totals(ObjectKind::Properties).count,
            0,
            "an object with room for its properties should never allocate an array"
        );
    }

    #[test]
    fn the_first_overflow_array_holds_more_than_the_one_property_that_caused_it() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        let x = name(&mut heap, "x");
        object.set(&mut heap, x, 1).expect("should have room");
        let totals = heap.census().totals(ObjectKind::Properties);
        assert_eq!(totals.count, 1);
        assert_eq!(
            totals.reserved_bytes,
            (PROPERTIES_HEADER + (FIRST_OVERFLOW as usize) * VALUE_SIZE) as u64
        );
    }

    #[test]
    fn a_small_integer_is_not_an_object() {
        assert_eq!(
            ObjectRef::from_slot(Slot::from_smi(3).expect("in range")),
            None
        );
    }
}
