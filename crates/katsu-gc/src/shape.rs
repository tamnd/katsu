//! What a set of objects with the same properties in the same order have in common.
//!
//! A shape says where a property lives without saying what its value is, so every object built the
//! same way shares one. That is the single decision the object model is built on: `{x: 1, y: 2}`
//! written a million times is a million objects and one shape, the name is stored once instead of a
//! million times, and a property read becomes an offset rather than a search once there is an inline
//! cache to remember the shape it saw.
//!
//! # Why the shapes form a tree
//!
//! A shape is not a description that objects point at, it is a node in a tree, and the edge from a
//! parent to a child is one property being added. The root is the empty object. `{}` with `x` added
//! is the root's child under `x`, and adding `y` to that reaches a grandchild. So two objects that
//! were built by adding the same names in the same order arrive at the same node without anything
//! comparing their property lists, because they walked the same path.
//!
//! The order is part of the identity and that is not an accident of the representation. JavaScript
//! specifies the enumeration order of string keys as insertion order, so `{x: 1, y: 2}` and
//! `{y: 2, x: 1}` are genuinely different objects and giving them one shape would be wrong rather
//! than clever.
//!
//! # What a lookup costs today
//!
//! Finding a name walks the parent chain comparing interned name slots, so it is a scan of the
//! object's own properties and nothing else, four bytes against four bytes with no dereference of
//! the string. That is linear in the number of properties, and it is the honest cost of a shape tree
//! with no descriptor array in it. The thing that removes it is not a hash table here, it is an
//! inline cache at the property access site that remembers the shape and the offset it found, which
//! is the whole reason the shape exists. A per shape table would be paying to make the slow path
//! faster when the design is that the slow path runs once per site.
//!
//! The transition lookup is a scan too, over the children of one shape, and it is bounded by how
//! many different names a program has ever added at that exact point in construction. For an object
//! literal that is one.

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};
use crate::object::{HeapKind, read_u32, slot_of, write_kind, write_u32, write_u32_at};
use crate::string::StringRef;

/// Bytes a shape occupies, which is the same for every shape because they all hold the same six
/// words: the kind tag, the count, the name, the parent, the first child and the next sibling.
pub(crate) const SHAPE_SIZE: usize = 24;
/// How many properties an object with this shape has, which is also the depth of this node.
const COUNT_OFFSET: usize = 4;
/// The name this shape added to its parent, or a small integer zero at the root.
const NAME_OFFSET: usize = 8;
/// The shape this one was reached from, or a small integer zero at the root.
const PARENT_OFFSET: usize = 12;
/// The first transition out of this shape, or a small integer zero if nothing was ever added to it.
const CHILD_OFFSET: usize = 16;
/// The next transition out of the same parent, which is how the children are chained together.
const SIBLING_OFFSET: usize = 20;

/// One node in the transition tree: a property layout that objects share.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShapeRef(Slot);

impl std::fmt::Debug for ShapeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shape@{:?}", self.0)
    }
}

impl ShapeRef {
    /// The shape of an object with no properties, which is the root every other shape hangs off.
    ///
    /// One per heap, held by whoever owns the heap, because two roots would mean two trees and two
    /// objects built identically could then have different shapes, which defeats the entire point.
    ///
    /// Returns `None` if the heap is full.
    #[must_use]
    pub fn root(heap: &mut BumpHeap) -> Option<ShapeRef> {
        let pointer = heap.allocate(SHAPE_SIZE, ObjectKind::Shape)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `SHAPE_SIZE` bytes and every field is inside it. The name, the
        // parent, the child and the sibling are all left as the zero the heap hands out, which is a
        // small integer zero and reads back as "there is none".
        unsafe {
            write_kind(pointer, HeapKind::Shape);
        }
        Some(ShapeRef(slot))
    }

    /// How many properties an object with this shape has.
    #[must_use]
    pub fn count(self, cage: &Cage) -> u32 {
        // SAFETY: the slot points at a shape, which is `SHAPE_SIZE` bytes long.
        unsafe { read_u32(cage, self.offset(), COUNT_OFFSET) }
    }

    /// The name this shape added, or `None` at the root, which added nothing.
    #[must_use]
    pub fn name(self, cage: &Cage) -> Option<StringRef> {
        StringRef::from_slot(Slot::from_bits(self.field(cage, NAME_OFFSET)))
    }

    /// The shape this one was reached from, or `None` at the root.
    #[must_use]
    pub fn parent(self, cage: &Cage) -> Option<ShapeRef> {
        ShapeRef::from_slot(Slot::from_bits(self.field(cage, PARENT_OFFSET)))
    }

    /// Where `name` sits in an object with this shape, or `None` if it has no such property.
    ///
    /// The index is a property number and not a byte offset, because where the value physically
    /// lives depends on how much room the object was built with rather than on the shape. See
    /// `ordinary.rs`.
    ///
    /// The comparison is between interned slots and not between strings, so a name that was not
    /// interned misses even when the text matches. Every caller in the runtime reaches this with a
    /// name that came out of the constant pool or the atom table, both of which intern.
    #[must_use]
    pub fn index_of(self, cage: &Cage, name: StringRef) -> Option<u32> {
        let wanted = name.slot().to_bits();
        let mut shape = self;
        loop {
            let count = shape.count(cage);
            if count == 0 {
                return None;
            }
            if shape.field(cage, NAME_OFFSET) == wanted {
                return Some(count - 1);
            }
            shape = shape.parent(cage)?;
        }
    }

    /// The shape an object of this shape has after `name` is added to it.
    ///
    /// Reuses the transition if this shape has been given that name before, and creates one
    /// otherwise, which is what makes two objects built the same way share a shape rather than
    /// merely have equal layouts.
    ///
    /// The caller is responsible for having asked [`ShapeRef::index_of`] first. Adding a name that
    /// is already in the chain would build a shape with the same name twice, where the second is
    /// unreachable, and that is a property being overwritten rather than added.
    ///
    /// Returns `None` if the heap is full.
    #[must_use]
    pub fn transition(self, heap: &mut BumpHeap, name: StringRef) -> Option<ShapeRef> {
        if let Some(existing) = self.child_named(heap.cage(), name) {
            return Some(existing);
        }
        let count = self.count(heap.cage()).checked_add(1)?;
        let first_child = self.field(heap.cage(), CHILD_OFFSET);

        let pointer = heap.allocate(SHAPE_SIZE, ObjectKind::Shape)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `SHAPE_SIZE` bytes and every field written is inside it.
        unsafe {
            write_kind(pointer, HeapKind::Shape);
            write_u32(pointer, COUNT_OFFSET, count);
            write_u32(pointer, NAME_OFFSET, name.slot().to_bits());
            write_u32(pointer, PARENT_OFFSET, self.0.to_bits());
            write_u32(pointer, SIBLING_OFFSET, first_child);
        }

        // The new child goes on the front of the parent's list, which is one store rather than a
        // walk to the end, and the order children are chained in is not observable.
        // SAFETY: `self` points at a shape, so its child field is inside the allocation, and
        // `&mut BumpHeap` means nothing else is reading the cage.
        unsafe {
            write_u32_at(heap.cage(), self.offset(), CHILD_OFFSET, slot.to_bits());
        }
        Some(ShapeRef(slot))
    }

    /// Every name an object with this shape has, in the order they were added.
    ///
    /// Insertion order is what `console.log` prints in and what `Object.keys` will return, so the
    /// chain is walked backwards and then reversed rather than being read out as it comes. It costs
    /// a vector, which is why this is the enumeration path and not the lookup path.
    #[must_use]
    pub fn names(self, cage: &Cage) -> Vec<StringRef> {
        let mut names = Vec::with_capacity(self.count(cage) as usize);
        let mut shape = self;
        while let Some(name) = shape.name(cage) {
            names.push(name);
            let Some(parent) = shape.parent(cage) else {
                break;
            };
            shape = parent;
        }
        names.reverse();
        names
    }

    /// The slot this shape lives at, for writing into an object's first word.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read a shape back out of a slot, or `None` if the slot is not a pointer.
    #[must_use]
    pub const fn from_slot(slot: Slot) -> Option<ShapeRef> {
        if slot.is_pointer() {
            Some(ShapeRef(slot))
        } else {
            None
        }
    }

    /// The transition out of this shape under `name`, if there has ever been one.
    fn child_named(self, cage: &Cage, name: StringRef) -> Option<ShapeRef> {
        let wanted = name.slot().to_bits();
        let mut child = ShapeRef::from_slot(Slot::from_bits(self.field(cage, CHILD_OFFSET)));
        while let Some(shape) = child {
            if shape.field(cage, NAME_OFFSET) == wanted {
                return Some(shape);
            }
            child = ShapeRef::from_slot(Slot::from_bits(shape.field(cage, SIBLING_OFFSET)));
        }
        None
    }

    /// One of the four slot fields, as raw bits.
    fn field(self, cage: &Cage, at: usize) -> u32 {
        // SAFETY: the slot points at a shape, and every offset this is called with is one of its
        // own fields, all of which are written before the shape escapes.
        unsafe { read_u32(cage, self.offset(), at) }
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{SHAPE_SIZE, ShapeRef};
    use crate::bump::{BumpHeap, ObjectKind};
    use crate::cage::Slot;
    use crate::object::HeapKind;
    use crate::string::StringRef;

    fn heap() -> BumpHeap {
        BumpHeap::new().expect("should reserve a cage")
    }

    fn name(heap: &mut BumpHeap, text: &str) -> StringRef {
        StringRef::from_str(heap, text).expect("should have room")
    }

    fn texts(heap: &BumpHeap, shape: ShapeRef) -> Vec<String> {
        shape
            .names(heap.cage())
            .into_iter()
            .map(|name| name.to_utf8_lossy(heap.cage()).into_owned())
            .collect()
    }

    #[test]
    fn the_root_is_the_shape_of_an_object_with_nothing_on_it() {
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        assert_eq!(root.count(heap.cage()), 0);
        assert_eq!(root.name(heap.cage()), None);
        assert_eq!(root.parent(heap.cage()), None);
        assert_eq!(
            HeapKind::of(heap.cage(), root.slot()),
            Some(HeapKind::Shape)
        );
    }

    #[test]
    fn adding_a_name_reaches_a_child_that_knows_where_the_name_went() {
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        let with_x = root.transition(&mut heap, x).expect("should have room");
        assert_eq!(with_x.count(heap.cage()), 1);
        assert_eq!(with_x.index_of(heap.cage(), x), Some(0));
        assert_eq!(root.index_of(heap.cage(), x), None);
        assert_eq!(with_x.parent(heap.cage()), Some(root));
    }

    #[test]
    fn two_objects_built_the_same_way_reach_one_shape() {
        // The property the whole design rests on. Nothing here compares property lists: both walks
        // take the same two edges out of the same root and arrive at the same node.
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        let y = name(&mut heap, "y");
        let first = root
            .transition(&mut heap, x)
            .and_then(|shape| shape.transition(&mut heap, y))
            .expect("should have room");
        let before = heap.census().totals(ObjectKind::Shape).count;
        let second = root
            .transition(&mut heap, x)
            .and_then(|shape| shape.transition(&mut heap, y))
            .expect("should have room");
        assert_eq!(first, second);
        assert_eq!(
            heap.census().totals(ObjectKind::Shape).count,
            before,
            "the second object should not have allocated a shape"
        );
    }

    #[test]
    fn the_same_names_in_the_other_order_are_a_different_shape() {
        // Insertion order is observable in JavaScript, so it is part of a shape's identity rather
        // than something the representation is free to normalise away.
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        let y = name(&mut heap, "y");
        let xy = root
            .transition(&mut heap, x)
            .and_then(|shape| shape.transition(&mut heap, y))
            .expect("should have room");
        let yx = root
            .transition(&mut heap, y)
            .and_then(|shape| shape.transition(&mut heap, x))
            .expect("should have room");
        assert_ne!(xy, yx);
        assert_eq!(xy.index_of(heap.cage(), x), Some(0));
        assert_eq!(yx.index_of(heap.cage(), x), Some(1));
    }

    #[test]
    fn one_shape_can_be_the_start_of_several_layouts() {
        // Two programs that build `{a: 1}` and then add different second properties share the work
        // of the first property and diverge at the second, which is what the tree is for.
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let a = name(&mut heap, "a");
        let b = name(&mut heap, "b");
        let c = name(&mut heap, "c");
        let with_a = root.transition(&mut heap, a).expect("should have room");
        let ab = with_a.transition(&mut heap, b).expect("should have room");
        let ac = with_a.transition(&mut heap, c).expect("should have room");
        assert_ne!(ab, ac);
        assert_eq!(ab.parent(heap.cage()), Some(with_a));
        assert_eq!(ac.parent(heap.cage()), Some(with_a));
        assert_eq!(texts(&heap, ab), ["a", "b"]);
        assert_eq!(texts(&heap, ac), ["a", "c"]);
    }

    #[test]
    fn the_names_come_back_in_the_order_they_were_added() {
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let mut shape = root;
        for text in ["first", "second", "third"] {
            let name = name(&mut heap, text);
            shape = shape.transition(&mut heap, name).expect("should have room");
        }
        assert_eq!(texts(&heap, shape), ["first", "second", "third"]);
        assert_eq!(texts(&heap, root), Vec::<String>::new());
    }

    #[test]
    fn a_name_that_was_never_added_is_absent_rather_than_wrong() {
        let mut heap = heap();
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        let missing = name(&mut heap, "nope");
        let with_x = root.transition(&mut heap, x).expect("should have room");
        assert_eq!(with_x.index_of(heap.cage(), missing), None);
    }

    #[test]
    fn a_shape_costs_one_object_and_twenty_four_bytes() {
        // The layout claim from the field list, checked rather than asserted in prose, because the
        // per shape cost is what decides whether a program with many small layouts is affordable.
        let mut heap = heap();
        let before = heap.census().totals(ObjectKind::Shape);
        let root = ShapeRef::root(&mut heap).expect("should have room");
        let x = name(&mut heap, "x");
        root.transition(&mut heap, x).expect("should have room");
        let after = heap.census().totals(ObjectKind::Shape);
        assert_eq!(after.count - before.count, 2);
        assert_eq!(
            after.reserved_bytes - before.reserved_bytes,
            (2 * SHAPE_SIZE) as u64,
            "a shape should not need padding"
        );
    }

    #[test]
    fn a_small_integer_is_not_a_shape() {
        assert_eq!(
            ShapeRef::from_slot(Slot::from_smi(3).expect("in range")),
            None
        );
    }
}
