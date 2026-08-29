//! Inline caches, one per property access site.
//!
//! A property read has to turn a name into a place. Doing that from nothing means walking up the
//! shape's parent chain comparing interned names, which is one compare per property and therefore
//! linear in how many the object has: about a nanosecond to find the name added last and about
//! fifteen to find the first of sixteen. The walk gives the same answer every time a site runs
//! against the same kind of object, and most sites see one kind of object for their whole life, so
//! the answer is worth writing down.
//!
//! # What one entry is
//!
//! A shape to compare and a position to read. Two objects with the same shape have the same
//! properties in the same order, with the same flags, inheriting from the same prototype, so the
//! position a name was found at for one of them is the position it is at for all of them.
//!
//! That the prototype is in the shape is the payoff for putting it there rather than in the object.
//! One comparison guards the whole chain above the object as well as the object itself. That the
//! flags are in the shape is the payoff for the same decision about attributes: a matched shape has
//! already established that the slot holds a plain value rather than a pair of functions, so the flag
//! read that accessors added to every property lookup is not on the cached path at all.
//!
//! A position rather than a byte offset, which is the one thing here that was measured into its
//! current form rather than designed into it. An offset is one load cheaper on a hit, because it says
//! outright where the value is, but two objects can share a shape and not share an inline capacity:
//! `{a: 1}` is built with room for one property and `x = {}; x.a = 1` is built with room for none and
//! keeps its property in the overflow array, and both reach the same shape. So an offset has to be
//! guarded by the capacity as well, which makes the key eight bytes instead of four, an entry sixteen
//! instead of eight, and every object built the second way a permanent miss. A position needs no
//! second guard, since it means the same thing whatever the capacity, and `ObjectRef::value_of` turns
//! it into an address with one compare against a header word the hit has already touched.
//!
//! # What this is not yet
//!
//! One shape and not four. The state progression the design calls for is uninitialized, monomorphic,
//! polymorphic up to four shapes, then megamorphic with a shared stub cache, and this is the first
//! state and the second one. A site that sees a second kind of object overwrites rather than growing
//! a list, so a genuinely polymorphic site pays the full walk every time and also pays for filling a
//! cache nobody hits. That is worth fixing with a measurement in hand rather than in advance.
//!
//! Own properties only. An inherited hit is guarded by the same word, because the prototype is in the
//! shape, but the property lives on a different object whose own shape can change without the
//! receiver's shape changing, so caching one needs the validity cell from the design and that is its
//! own piece of work.
//!
//! Reads only. A store that grows an object changes its shape, so a store cache holds a transition
//! rather than a position and is a different entry with a different fill.

use std::cell::Cell;

use katsu_ir::CacheIndex;

/// What one access site remembers about the last object it read a property from.
///
/// Four bytes of key and four of payload. Eight is an eighth of a cache line, and it is small enough
/// that a function's whole table of them is a rounding error next to its bytecode, which matters
/// because the table is allocated whether or not any site in the function ever fills one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropertyCache {
    /// The shape this entry was filled from, or zero for an entry that has never been filled.
    ///
    /// Zero cannot collide with a real object. It is a slot, every pointer slot has its low bit set,
    /// so no object anywhere in the cage has a shape word of zero and an empty entry misses without
    /// needing a flag of its own.
    guard: u32,
    /// Which of the object's properties this is, valid only under a matching shape.
    index: u32,
}

impl PropertyCache {
    /// The property position to read, if this entry was filled by an object with this shape.
    #[inline]
    #[must_use]
    pub fn hit(self, guard: u32) -> Option<u32> {
        if self.guard == guard {
            Some(self.index)
        } else {
            None
        }
    }

    /// The entry to write after a lookup that went the long way.
    #[must_use]
    pub fn filled(guard: u32, index: u32) -> PropertyCache {
        PropertyCache { guard, index }
    }
}

/// One access site, which is a function's table of entries and this site's place in it.
///
/// The two travel together everywhere, because neither means anything without the other, and an
/// access site is the thing the rest of the runtime actually talks about. Copied rather than
/// borrowed: it is a pointer and an index, which is what passing a reference to it would cost
/// anyway.
#[derive(Clone, Copy, Debug)]
pub struct Site<'a> {
    caches: &'a Caches,
    index: u32,
}

impl<'a> Site<'a> {
    /// The site at `index` in `caches`.
    #[inline]
    #[must_use]
    pub fn new(caches: &'a Caches, index: CacheIndex) -> Site<'a> {
        Site {
            caches,
            index: index.0,
        }
    }

    /// The property position this site learned for `guard`, if it learned one.
    #[inline]
    #[must_use]
    pub fn hit(self, guard: u32) -> Option<u32> {
        self.caches.get(self.index).hit(guard)
    }

    /// Remember that an object with this shape keeps the property at this position.
    #[inline]
    pub fn fill(self, guard: u32, index: u32) {
        self.caches
            .set(self.index, PropertyCache::filled(guard, index));
    }
}

/// The caches of one function, one per site, shared by every frame running that function.
///
/// Shared rather than per frame because that is the whole point. A site inside a function called a
/// million times is the same site each time and what it learned on the first call is what the
/// millionth wants. Recursion shares them too, which is correct for the same reason: two frames of
/// the same function running the same site are reading the same kind of object or they are not, and
/// the entry says which.
///
/// [`Cell`] rather than a mutable borrow because a cache is written during execution and the code it
/// belongs to is borrowed for the length of the loop. A cache write is not a change to the program,
/// so it should not need the loop to give up its view of the program to make one.
#[derive(Debug, Default)]
pub struct Caches(Box<[Cell<PropertyCache>]>);

impl Caches {
    /// Room for `slots` sites, all empty.
    #[must_use]
    pub fn new(slots: u32) -> Caches {
        Caches(vec![Cell::new(PropertyCache::default()); slots as usize].into_boxed_slice())
    }

    /// The entry at `index`, or an empty one if the index is past the end.
    ///
    /// Answering rather than panicking on a bad index because a cache is an optimisation and a
    /// missing entry is a miss. Lowering counts the slots it hands out, so a bad index means the two
    /// disagree, and the cost of that should be a slow property read rather than a dead runtime.
    #[inline]
    #[must_use]
    pub fn get(&self, index: u32) -> PropertyCache {
        self.0
            .get(index as usize)
            .map_or_else(PropertyCache::default, Cell::get)
    }

    /// Write the entry at `index`, doing nothing if the index is past the end.
    #[inline]
    pub fn set(&self, index: u32, entry: PropertyCache) {
        if let Some(slot) = self.0.get(index as usize) {
            slot.set(entry);
        }
    }

    /// How many sites this holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this holds no sites, which is true of a function with no property access in it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Caches, PropertyCache};

    #[test]
    fn an_entry_that_was_never_filled_misses_whatever_it_is_asked_about() {
        let empty = PropertyCache::default();
        assert_eq!(empty.hit(0x0001), None);
        // Zero is the one word it would answer to, and no object produces it, because a shape word
        // is a pointer slot and a pointer slot always has its low bit set.
        assert_eq!(empty.hit(0), Some(0));
    }

    #[test]
    fn a_filled_entry_answers_only_the_shape_it_was_filled_from() {
        let entry = PropertyCache::filled(0x0005, 3);
        assert_eq!(entry.hit(0x0005), Some(3));
        assert_eq!(entry.hit(0x0007), None);
    }

    #[test]
    fn a_site_past_the_end_misses_rather_than_panicking() {
        let caches = Caches::new(2);
        assert_eq!(caches.len(), 2);
        caches.set(7, PropertyCache::filled(0x0001, 1));
        assert_eq!(caches.get(7).hit(0x0001), None);
    }

    #[test]
    fn a_function_with_no_sites_holds_none() {
        let caches = Caches::new(0);
        assert!(caches.is_empty());
        assert_eq!(caches.get(0).hit(0x0001), None);
    }

    #[test]
    fn what_a_site_learns_is_what_the_next_read_of_it_finds() {
        let caches = Caches::new(1);
        assert_eq!(caches.get(0).hit(0x0003), None);
        caches.set(0, PropertyCache::filled(0x0003, 1));
        assert_eq!(caches.get(0).hit(0x0003), Some(1));
        // A second kind of object overwrites rather than joining, which is what monomorphic means
        // and is the thing the polymorphic state is for.
        caches.set(0, PropertyCache::filled(0x0005, 2));
        assert_eq!(caches.get(0).hit(0x0003), None);
        assert_eq!(caches.get(0).hit(0x0005), Some(2));
    }
}
