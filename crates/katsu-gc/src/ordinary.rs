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
//! [shape][properties][inline capacity][elements][inline slot 0][inline slot 1]...
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
//! The elements word is the indexed properties, the ones a program reaches with `a[0]`, and
//! `elements.rs` is what it points at. It is in the fourth header word rather than the second the
//! spec drew it in, and that is not an accident: the header was already sixteen bytes because a
//! value has to start eight byte aligned, so the fourth word was padding that every object was
//! already paying for. Putting elements there costs nothing at all and moves no offset. An object
//! with no indexed properties has a zero there, which is what the heap already handed out, so it
//! costs nothing to write either.
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
//! # Where the prototype is
//!
//! Not here. It is in the shape, and `shape.rs` says why at length: it makes one shape comparison
//! enough to guard an inherited property, which is what an inline cache needs. So an object carries
//! no prototype word of its own and asking one for its prototype is a load of the shape and a load
//! out of it.
//!
//! # Where the attributes are
//!
//! Also not here, and also in the shape, for the same kind of reason. Each shape node adds one
//! property, so the three flags that property carries belong on the node that added it. What that
//! means for this file is that a property is a name and a set of flags in the shape and an eight
//! byte value here, and nothing in the layout below changed to make attributes work.
//!
//! # What is not here
//!
//! Accessors do not change the layout below, which is the point of how they were added. A property
//! is still one slot, and an accessor's slot holds one boxed pair of functions rather than two slots
//! holding one function each, so nothing that steps through properties has to ask how wide one is.
//! The flag that says the slot means a pair is in the shape with the other three, and the pair
//! itself is in `function.rs`. No delete, which is the one operation a transition tree genuinely does
//! not want and which needs the dictionary mode that every engine falls back to. That is its own
//! piece of work and it is in M1.

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};
use crate::elements::{ElementsRef, HOLE, NAMED, Stored};
use crate::object::{
    HeapKind, read_u32, read_u64, slot_of, write_kind, write_u32, write_u32_at, write_u64,
};
use crate::shape::{Attributes, ShapeRef};
use crate::string::StringRef;

/// Where the shape goes, which is the word every object in the cage starts with.
const SHAPE_OFFSET: usize = 0;
/// The overflow array, or a small integer zero when everything fits inside the object.
const PROPERTIES_OFFSET: usize = 4;
/// How many values fit inside the object.
const INLINE_OFFSET: usize = 8;
/// The indexed properties, or a small integer zero when this object has none.
///
/// The fourth word rather than the second, because the first three plus alignment already made the
/// header sixteen bytes, so this one was padding an object was paying for whether it used it or not.
const ELEMENTS_OFFSET: usize = 12;
/// Bytes before the first inline value. Four header words, which is also the alignment a value
/// needs, so nothing is padding any more.
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

    /// What this object inherits from, or `None` if it inherits from nothing.
    ///
    /// A load of the shape and a load out of it, because the prototype lives in the shape. See
    /// `shape.rs`.
    #[must_use]
    pub fn prototype(self, cage: &Cage) -> Option<ObjectRef> {
        self.shape(cage).prototype(cage)
    }

    /// The value stored under `name` on this object itself, or `None` if it has none.
    ///
    /// Own properties only, which is `Object.getOwnPropertyDescriptor` and not `o.name`. Walking the
    /// prototype chain is the interpreter's job rather than this one's, because a chain walk is a
    /// language operation with its own rules and this is the layer that knows where bytes are.
    #[must_use]
    pub fn get(self, cage: &Cage, name: StringRef) -> Option<u64> {
        let index = self.shape(cage).index_of(cage, name)?;
        self.value_at(cage, index)
    }

    /// Where `name` sits on this object itself and what it is allowed to do.
    ///
    /// Own properties only, like [`ObjectRef::get`], and for the same reason. This is what the
    /// interpreter asks at each step of a chain walk when it is deciding whether a write is allowed,
    /// because that decision is about the flags of the first copy of the name it meets rather than
    /// about the object it started from.
    #[must_use]
    pub fn find(self, cage: &Cage, name: StringRef) -> Option<(u32, Attributes)> {
        self.shape(cage).find(cage, name)
    }

    /// Store `value` under `name`, adding the property if the object does not have it.
    ///
    /// This is where an object grows a shape. Adding a property takes a transition, and taking the
    /// same transition from the same shape twice reaches the same node, so the millionth object
    /// built like the first shares its layout. A property that is added arrives with
    /// [`Attributes::DEFAULT`], which is what an assignment and a literal produce, and a property
    /// that is already there keeps the flags it has.
    ///
    /// It does not check whether the property is writable, and that is deliberate rather than
    /// missing. Refusing a write is a language decision with a strict mode rule attached to it and a
    /// prototype chain to walk first, so it belongs to the interpreter, and this is the layer that
    /// knows where bytes are. Every caller in the runtime asks [`ObjectRef::find`] first.
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
        self.add(heap, shape, name, value, Attributes::DEFAULT)
    }

    /// Put `name` on this object with exactly `attributes`, whether or not it is already there.
    ///
    /// This is `[[DefineOwnProperty]]` for a data property, minus the part that decides whether the
    /// change is allowed, which needs the old flags and a `TypeError` and is therefore the
    /// interpreter's. Redefining a property that is already there and giving it different flags is
    /// the expensive case, because it rebuilds the layout from the root of the transition tree, and
    /// `shape.rs` says why that is the honest answer rather than a shortcut being avoided.
    ///
    /// Returns `None` if the heap is full.
    #[must_use]
    pub fn define(
        self,
        heap: &mut BumpHeap,
        name: StringRef,
        value: u64,
        attributes: Attributes,
    ) -> Option<()> {
        let shape = self.shape(heap.cage());
        let Some((index, existing)) = shape.find(heap.cage(), name) else {
            return self.add(heap, shape, name, value, attributes);
        };
        if existing != attributes {
            let reshaped = shape.redefine(heap, index, attributes)?;
            // SAFETY: the object is in the cage and its first word is inside it, and `&mut BumpHeap`
            // means nothing else is reading the cage.
            unsafe {
                write_u32_at(
                    heap.cage(),
                    self.offset(),
                    SHAPE_OFFSET,
                    reshaped.slot().to_bits(),
                );
            }
        }
        self.write(heap, index, value);
        Some(())
    }

    /// Add a property this object does not have, which is the half of a store that grows a shape.
    fn add(
        self,
        heap: &mut BumpHeap,
        shape: ShapeRef,
        name: StringRef,
        value: u64,
        attributes: Attributes,
    ) -> Option<()> {
        let index = shape.count(heap.cage());
        let grown = shape.transition(heap, name, attributes)?;
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

    /// Every property anything walking this object is allowed to see: its name and where it is.
    ///
    /// What `console.log`, `JSON.stringify`, `Object.keys` and `for in` want, as against
    /// [`ObjectRef::names`], which is what `Object.getOwnPropertyNames` wants. Two methods rather
    /// than one with a flag, because the two have different callers and mixing them up is a bug that
    /// shows up as a builtin method appearing in someone's printed output.
    ///
    /// The index comes back with the name rather than being the position in this list, because a
    /// hidden property still occupies a slot. Counting the visible ones and using that as an index
    /// would read the wrong value for every property after the first hidden one, which is the sort of
    /// bug that stays invisible until a builtin is installed next to a real property.
    /// The flags come back too, because the caller cannot read the slot without them. An accessor's
    /// slot holds a pair of functions rather than a value, and both callers of this need to know
    /// which they are looking at before they touch it.
    #[must_use]
    pub fn enumerable(self, cage: &Cage) -> Vec<(StringRef, u32, Attributes)> {
        self.shape(cage)
            .entries(cage)
            .into_iter()
            .enumerate()
            .filter_map(|(index, (name, attributes))| {
                let index = u32::try_from(index).ok()?;
                attributes
                    .is_enumerable()
                    .then_some((name, index, attributes))
            })
            .collect()
    }

    /// The word an inline cache compares, which is this object's shape.
    ///
    /// Two objects with the same word have the same properties in the same order, with the same
    /// flags, inheriting from the same prototype. That is everything a site needs to be sure that the
    /// position it found a name at last time is the position it would find it at now, which is why a
    /// cache can key on this one word and read nothing else about the layout.
    ///
    /// The raw slot bits rather than a [`ShapeRef`], because a cache stores what it compares and
    /// building a reference to a shape nothing is going to read is work with no answer in it.
    #[must_use]
    pub fn guard(self, cage: &Cage) -> u32 {
        self.field(cage, SHAPE_OFFSET)
    }

    /// The value of the property at a position that a matched guard word has already established.
    ///
    /// The same read [`ObjectRef::value_at`] does without the two bounds tests, because a site whose
    /// guard word matched knows the shape, and the shape is what says how many properties there are.
    /// Both tests are loads of their own, and the second one goes through the shape, so skipping them
    /// is most of what makes a hit cheaper than a search.
    ///
    /// The inline capacity is still read, because it is the one thing about the layout the shape does
    /// not fix: `{a: 1}` is built with room for one and `x = {}; x.a = 1` is built with room for
    /// none, and both reach the same shape. That is a load and a compare off a header word that is
    /// already in cache, and paying it here is what lets an overflow property be cached at all.
    ///
    /// # Panics
    ///
    /// Never, for an index that came from [`ObjectRef::find`] on a matching shape. An index from
    /// anywhere else is a bug, and the expectation below says so rather than reading a wrong word.
    #[must_use]
    pub fn value_of(self, cage: &Cage, index: u32) -> u64 {
        let inline = self.inline(cage);
        if index < inline {
            // SAFETY: the index is below the inline capacity the header records, so the value is
            // inside the allocation, and the shape it came from says it has been written.
            return unsafe {
                read_u64(
                    cage.address_of(self.offset()),
                    HEADER_SIZE + (index as usize) * VALUE_SIZE,
                )
            };
        }
        let properties = self
            .properties(cage)
            .expect("a property past the inline room is a property in the overflow array");
        // SAFETY: the shape says the object has this property, and everything the shape counts that
        // does not fit inside the object has room in the array.
        unsafe {
            read_u64(
                cage.address_of(properties.offset()),
                PROPERTIES_HEADER + ((index - inline) as usize) * VALUE_SIZE,
            )
        }
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

    /// This object's indexed properties, if it has any.
    ///
    /// `None` for every object that has only named properties on it, which is most of them, and the
    /// check for it is a tag test on a word that is in the same cache line as the shape.
    #[must_use]
    pub fn elements(self, cage: &Cage) -> Option<ElementsRef> {
        ElementsRef::from_slot(Slot::from_bits(self.field(cage, ELEMENTS_OFFSET)))
    }

    /// The value at `index`, or [`HOLE`] if there is nothing there.
    ///
    /// This is the whole of reading `a[i]` once the index is known to be an integer: a load of the
    /// elements word, a bounds check, and a load. No string, no hash, no shape.
    #[must_use]
    pub fn element(self, cage: &Cage, index: u32) -> u64 {
        self.elements(cage)
            .map_or(HOLE, |array| array.value_at(cage, index))
    }

    /// Give this object room for `capacity` indexed properties, replacing whatever it had.
    ///
    /// For a caller that knows the count before it writes anything, which is what an array literal
    /// is: `[1, 2, 3]` allocates once for three values rather than growing three times. Any elements
    /// the object already had are dropped, so this is for a fresh object and not for a resize.
    pub fn reserve_elements(self, heap: &mut BumpHeap, capacity: u32) -> Option<ElementsRef> {
        let array = ElementsRef::new(heap, capacity)?;
        self.point_at(heap, array);
        Some(array)
    }

    /// Write the value of the indexed property at `index`, growing the elements array if it has to.
    ///
    /// The answer is not always yes. An index far enough past the end is refused rather than stored,
    /// because the array that would hold it is almost entirely holes, and the caller is expected to
    /// put the value under the text of the number instead. `elements.rs` has the reasoning and the
    /// number. An index that is already marked as living under a name is refused for the same
    /// reason and to the same place, which is what keeps it from being one property in two.
    ///
    /// The read that the refusal needs is on the same cache line the write is about to dirty, so it
    /// costs a comparison and not a memory access.
    pub fn set_element(self, heap: &mut BumpHeap, index: u32, value: u64) -> Stored {
        let existing = self.elements(heap.cage());
        let capacity = existing.map_or(0, |array| array.capacity(heap.cage()));
        if let Some(array) = existing
            && index < capacity
        {
            if array.value_at(heap.cage(), index) == NAMED {
                return Stored::Named;
            }
            array.set(heap, index, value);
            return Stored::Yes;
        }
        let Some(grown) = ElementsRef::grown_for(capacity, index) else {
            return Stored::TooSparse;
        };
        let Some(array) = ElementsRef::new(heap, grown) else {
            return Stored::NoRoom;
        };
        for slot in 0..capacity {
            let old = existing
                .expect("a capacity above zero means there is an array to read it from")
                .value_at(heap.cage(), slot);
            array.set(heap, slot, old);
        }
        self.point_at(heap, array);
        array.set(heap, index, value);
        Stored::Yes
    }

    /// Record that `index` is a property of this object under the text of the number.
    ///
    /// Answers false when there is no room in the elements array for the mark, which is the state an
    /// index too sparse for an array is already in and needs nothing else to stay in. The mark is
    /// only needed once the array grows out over the index, and the caller that grows it is the one
    /// that puts it there.
    pub fn mark_named(self, heap: &mut BumpHeap, index: u32) -> bool {
        let Some(array) = self.elements(heap.cage()) else {
            return false;
        };
        if index >= array.capacity(heap.cage()) {
            return false;
        }
        array.set(heap, index, NAMED);
        true
    }

    /// Point the elements word at `array`.
    fn point_at(self, heap: &mut BumpHeap, array: ElementsRef) {
        // SAFETY: the object is in the cage and its elements word is inside its header.
        unsafe {
            write_u32_at(
                heap.cage(),
                self.offset(),
                ELEMENTS_OFFSET,
                array.slot().to_bits(),
            );
        }
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

    /// Write the value of a property whose index is already known.
    ///
    /// For a caller that has just asked [`ObjectRef::find`] where a name is and does not want to ask
    /// again. The index has to have come from this object, because it is a position in this object's
    /// layout and nothing checks that it is one. Writing past the end is not unsafe, it just puts the
    /// value somewhere no name points at, which is a silently lost write and worse than a crash.
    pub fn write_at(self, heap: &mut BumpHeap, index: u32, value: u64) {
        self.write(heap, index, value);
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
    use super::{
        Attributes, FIRST_OVERFLOW, HEADER_SIZE, HOLE, NAMED, ObjectRef, PROPERTIES_HEADER, Stored,
        VALUE_SIZE,
    };
    use crate::bump::{BumpHeap, ObjectKind};
    use crate::cage::Slot;
    use crate::elements::FIRST_ELEMENTS;
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
        let root = ShapeRef::root(heap, None).expect("should have room");
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
        let root = ShapeRef::root(&mut heap, None).expect("should have room");
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
        let root = ShapeRef::root(&mut heap, None).expect("should have room");
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
    fn a_property_can_be_added_with_flags_of_its_own() {
        let mut heap = heap();
        let object = empty(&mut heap, 2);
        let shown = name(&mut heap, "shown");
        let hidden = name(&mut heap, "hidden");
        object.set(&mut heap, shown, 1).expect("should have room");
        object
            .define(&mut heap, hidden, 2, Attributes::BUILTIN)
            .expect("should have room");
        assert_eq!(object.get(heap.cage(), hidden), Some(2));
        assert_eq!(texts(&heap, object), ["shown", "hidden"]);
        assert_eq!(
            object
                .enumerable(heap.cage())
                .into_iter()
                .map(|(name, _, _)| name.to_utf8_lossy(heap.cage()).into_owned())
                .collect::<Vec<String>>(),
            ["shown"],
            "a non enumerable property is on the object and not in the walk of it"
        );
    }

    #[test]
    fn redefining_a_property_changes_its_flags_and_keeps_its_value_in_place() {
        // The case that rebuilds the shape from the root. The value must survive it, and so must
        // every other property's index, because the rebuild does not move anything.
        let mut heap = heap();
        let object = empty(&mut heap, 3);
        let names: Vec<StringRef> = ["a", "b", "c"]
            .into_iter()
            .map(|text| name(&mut heap, text))
            .collect();
        for (index, &name) in names.iter().enumerate() {
            object
                .set(&mut heap, name, index as u64)
                .expect("should have room");
        }
        object
            .define(&mut heap, names[0], 99, Attributes::NONE)
            .expect("should have room");
        assert_eq!(object.get(heap.cage(), names[0]), Some(99));
        assert_eq!(object.get(heap.cage(), names[1]), Some(1));
        assert_eq!(object.get(heap.cage(), names[2]), Some(2));
        assert_eq!(texts(&heap, object), ["a", "b", "c"]);
        assert_eq!(
            object.find(heap.cage(), names[0]),
            Some((0, Attributes::NONE))
        );
    }

    #[test]
    fn a_plain_assignment_leaves_the_flags_of_a_property_alone() {
        // What separates `set` from `define`. An assignment to a property that exists is not a
        // redefinition of it, so a non enumerable property that gets written stays non enumerable,
        // and the object does not change shape.
        let mut heap = heap();
        let object = empty(&mut heap, 1);
        let x = name(&mut heap, "x");
        object
            .define(&mut heap, x, 1, Attributes::BUILTIN)
            .expect("should have room");
        let shape = object.shape(heap.cage());
        object.set(&mut heap, x, 2).expect("should have room");
        assert_eq!(object.get(heap.cage(), x), Some(2));
        assert_eq!(object.find(heap.cage(), x), Some((0, Attributes::BUILTIN)));
        assert_eq!(object.shape(heap.cage()), shape);
    }

    #[test]
    fn defining_a_property_that_is_already_the_way_it_is_asked_for_is_a_write() {
        // The cheap half of `define`: same flags means no rebuild, so the shape does not move and
        // nothing is allocated.
        let mut heap = heap();
        let object = empty(&mut heap, 1);
        let x = name(&mut heap, "x");
        object
            .define(&mut heap, x, 1, Attributes::NONE)
            .expect("should have room");
        let shape = object.shape(heap.cage());
        let before = heap.census().totals(ObjectKind::Shape).count;
        object
            .define(&mut heap, x, 2, Attributes::NONE)
            .expect("should have room");
        assert_eq!(object.shape(heap.cage()), shape);
        assert_eq!(heap.census().totals(ObjectKind::Shape).count, before);
        assert_eq!(object.get(heap.cage(), x), Some(2));
    }

    #[test]
    fn a_small_integer_is_not_an_object() {
        assert_eq!(
            ObjectRef::from_slot(Slot::from_smi(3).expect("in range")),
            None
        );
    }

    #[test]
    fn an_object_starts_with_no_indexed_properties_and_nobody_wrote_that() {
        let mut heap = heap();
        let object = empty(&mut heap, 2);
        assert_eq!(object.elements(heap.cage()), None);
        assert_eq!(object.element(heap.cage(), 0), HOLE);
    }

    #[test]
    fn an_indexed_property_that_was_put_in_comes_back_out() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        assert_eq!(object.set_element(&mut heap, 0, 11), Stored::Yes);
        assert_eq!(object.set_element(&mut heap, 2, 33), Stored::Yes);
        assert_eq!(object.element(heap.cage(), 0), 11);
        assert_eq!(object.element(heap.cage(), 1), HOLE);
        assert_eq!(object.element(heap.cage(), 2), 33);
    }

    #[test]
    fn an_index_and_a_name_are_two_different_places() {
        let mut heap = heap();
        let object = empty(&mut heap, 1);
        let zero = name(&mut heap, "0");
        object
            .define(&mut heap, zero, 11, Attributes::NONE)
            .expect("should have room");
        object.set_element(&mut heap, 0, 22);
        assert_eq!(object.get(heap.cage(), zero), Some(11));
        assert_eq!(object.element(heap.cage(), 0), 22);
        assert_eq!(texts(&heap, object), vec!["0"]);
    }

    #[test]
    fn growing_the_elements_keeps_the_values_that_were_already_there() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        for index in 0..64 {
            assert_eq!(
                object.set_element(&mut heap, index, u64::from(index) + 1),
                Stored::Yes
            );
        }
        for index in 0..64 {
            assert_eq!(object.element(heap.cage(), index), u64::from(index) + 1);
        }
    }

    #[test]
    fn growing_the_elements_leaves_the_new_room_as_holes() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        object.set_element(&mut heap, 0, 11);
        let array = object.elements(heap.cage()).expect("should have elements");
        assert_eq!(array.capacity(heap.cage()), FIRST_ELEMENTS);
        assert_eq!(array.used(heap.cage()), Some(1));
        assert_eq!(object.element(heap.cage(), 3), HOLE);
    }

    #[test]
    fn an_index_far_past_the_end_is_refused_rather_than_stored() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        assert_eq!(object.set_element(&mut heap, 100_000, 1), Stored::TooSparse);
        assert_eq!(object.elements(heap.cage()), None);
    }

    #[test]
    fn a_marked_index_refuses_a_write_so_the_name_keeps_it() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        object.set_element(&mut heap, 3, 11);
        assert!(object.mark_named(&mut heap, 3));
        assert_eq!(object.set_element(&mut heap, 3, 22), Stored::Named);
        assert_eq!(object.element(heap.cage(), 3), NAMED);
    }

    #[test]
    fn marking_an_index_the_array_does_not_reach_answers_false() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        // Nothing to mark and nothing that needs it: the index is already outside every array this
        // object has, so a write there takes the slow path anyway.
        assert!(!object.mark_named(&mut heap, 3));
        object.set_element(&mut heap, 0, 11);
        assert!(!object.mark_named(&mut heap, 100));
    }

    #[test]
    fn a_mark_is_not_a_value_so_nothing_counts_it_as_one() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        object.set_element(&mut heap, 0, 11);
        object.set_element(&mut heap, 1, 22);
        object.mark_named(&mut heap, 1);
        let elements = object.elements(heap.cage()).expect("should have an array");
        assert_eq!(elements.used(heap.cage()), Some(1));
    }

    #[test]
    fn a_literal_gets_the_room_it_asked_for_in_one_allocation() {
        let mut heap = heap();
        let object = empty(&mut heap, 0);
        let before = heap.census().totals(ObjectKind::Elements).count;
        object
            .reserve_elements(&mut heap, 3)
            .expect("should have room");
        object.set_element(&mut heap, 0, 11);
        object.set_element(&mut heap, 1, 22);
        object.set_element(&mut heap, 2, 33);
        assert_eq!(heap.census().totals(ObjectKind::Elements).count - before, 1);
        assert_eq!(object.element(heap.cage(), 2), 33);
    }

    #[test]
    fn the_elements_word_costs_an_object_nothing() {
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap, None).expect("should have room");
        let before = heap.cursor();
        ObjectRef::new(&mut heap, root, 2).expect("should have room");
        assert_eq!(heap.cursor() - before, HEADER_SIZE + 2 * VALUE_SIZE);
    }
}
