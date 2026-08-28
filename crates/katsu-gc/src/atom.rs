//! The atom table: one canonical string per distinct piece of text.
//!
//! `spec/07-object-model.md` 7.7 asks for interned atoms for property names so that `"length"` is
//! one comparison rather than six. That is the whole point of this file. Every property name, every
//! identifier out of the parser and every constant pool string goes through here, and what comes
//! back is an [`Atom`], which is four bytes and compares as an integer.
//!
//! The table itself is open addressed with linear probing. Linear probing rather than anything
//! cleverer because the table is small, the entries are four bytes, and a probe that stays in the
//! same cache line beats a probe sequence that is theoretically shorter. The load factor is kept
//! at three quarters, which is where linear probing still has short runs.
//!
//! One detail is worth pointing at, because it is the second time the same invariant has paid off.
//! An empty entry is a slot of all zero bits. `spec/07-object-model.md` 7.2.1 says a zero slot is
//! the integer zero and never a pointer, so a freshly zeroed table is an empty table and growing
//! the table does not need a pass to write a sentinel into every bucket.
//!
//! The table's own memory is a Rust `Vec` outside the cage, so the heap census does not see it.
//! That is a hole in the accounting rather than a design, and [`AtomTable::memory_bytes`] exists
//! so that whoever reports the numbers can add it back in. M1 moves the table into the realm
//! snapshot, which is where 7.7 says it belongs and where the census will see it for free.

use std::fmt;

use crate::bump::BumpHeap;
use crate::cage::{Cage, Slot};
use crate::string::{StringRef, hash_str};

/// The smallest table worth allocating, in buckets. Must be a power of two.
const MIN_CAPACITY: usize = 16;

/// An interned string: the canonical copy of some text, for the lifetime of the heap.
///
/// Two atoms from the same table are equal exactly when their text is equal, and the comparison is
/// four bytes wide. That is the property inline caches are built on, so it is worth having a
/// distinct type for it rather than passing around a [`StringRef`] and remembering.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Atom(Slot);

impl fmt::Debug for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same limitation as `StringRef`: no cage, so no characters.
        write!(f, "atom@{:?}", self.0)
    }
}

impl Atom {
    /// The string behind this atom.
    #[must_use]
    pub const fn as_string(self) -> StringRef {
        match StringRef::from_slot(self.0) {
            Some(string) => string,
            // An atom is only ever built from an interned string, so this cannot happen. Saying
            // so with a panic rather than a fallback keeps the invariant checkable.
            None => panic!("an atom always names a string"),
        }
    }

    /// The compressed slot, for writing into an object or a constant pool.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// The text, when it is ASCII and can be borrowed for free.
    #[must_use]
    pub fn as_ascii(self, cage: &Cage) -> Option<&str> {
        self.as_string().as_ascii(cage)
    }
}

/// A table of interned strings.
///
/// Not a `HashMap<String, _>`. The keys live in the cage as real JavaScript strings, because that
/// is what a property name has to be anyway, and keeping a second UTF-8 copy of every identifier
/// in a Rust map would be paying twice for the thing this engine is trying to be cheap about.
pub struct AtomTable {
    /// One slot per bucket. A zero slot is an empty bucket, per the module comment.
    buckets: Vec<Slot>,
    /// How many buckets are occupied.
    occupied: usize,
}

impl Default for AtomTable {
    fn default() -> AtomTable {
        AtomTable::new()
    }
}

impl fmt::Debug for AtomTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomTable")
            .field("atoms", &self.occupied)
            .field("buckets", &self.buckets.len())
            .finish()
    }
}

impl AtomTable {
    /// An empty table that has allocated nothing.
    ///
    /// A process that never interns a string pays nothing for this, which matters because the
    /// startup budget in `spec/02-the-10x-goal.md` is measured on a program that does almost
    /// nothing.
    #[must_use]
    pub const fn new() -> AtomTable {
        AtomTable {
            buckets: Vec::new(),
            occupied: 0,
        }
    }

    /// How many distinct atoms are in the table.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.occupied
    }

    /// Whether nothing has been interned yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    /// Bytes the table itself occupies, outside the cage and outside the heap census.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.buckets.capacity() * size_of::<Slot>()
    }

    /// Find the atom for some text, without allocating anything if it is missing.
    ///
    /// This is the lookup a property access does. It hashes the text directly rather than building
    /// a string to hash, which is why [`crate::hash_str`] is defined over code units.
    #[must_use]
    pub fn lookup(&self, cage: &Cage, text: &str) -> Option<Atom> {
        if self.buckets.is_empty() {
            return None;
        }
        let hash = hash_str(text);
        let mask = self.buckets.len() - 1;
        let mut index = hash as usize & mask;
        loop {
            let slot = self.buckets[index];
            let Some(existing) = StringRef::from_slot(slot) else {
                // An integer slot is an empty bucket, and linear probing stops at the first one.
                return None;
            };
            // The cached hash first, because it rejects a collision in one comparison and the
            // full comparison walks the characters.
            if existing.hash(cage) == hash && existing.equals_str(cage, text) {
                return Some(Atom(slot));
            }
            index = (index + 1) & mask;
        }
    }

    /// Find the atom for some text, allocating the canonical string if it is not there yet.
    ///
    /// Returns `None` only when the heap is full, which at M0 is unrecoverable.
    pub fn intern(&mut self, heap: &mut BumpHeap, text: &str) -> Option<Atom> {
        if let Some(atom) = self.lookup(heap.cage(), text) {
            return Some(atom);
        }
        let string = StringRef::from_str(heap, text)?;
        Some(self.insert(heap.cage(), string))
    }

    /// Intern a string that is already in the heap.
    ///
    /// Returns the canonical copy, which may be a string interned earlier, in which case the one
    /// passed in is garbage. There is no collector yet, so it stays there. That is the M0 bargain
    /// and it is one more reason the parser should intern identifiers as it reads them rather than
    /// allocating first and interning afterwards.
    pub fn intern_string(&mut self, cage: &Cage, string: StringRef) -> Atom {
        let hash = string.hash(cage);
        if let Some(found) = self.find(cage, hash, |candidate| candidate.equals(cage, string)) {
            return found;
        }
        self.insert(cage, string)
    }

    /// Every atom in the table, in bucket order, which is not a useful order to anybody but a test.
    #[must_use]
    pub fn atoms(&self) -> Vec<Atom> {
        self.buckets
            .iter()
            .filter(|slot| slot.is_pointer())
            .map(|&slot| Atom(slot))
            .collect()
    }

    fn find(&self, cage: &Cage, hash: u32, matches: impl Fn(StringRef) -> bool) -> Option<Atom> {
        if self.buckets.is_empty() {
            return None;
        }
        let mask = self.buckets.len() - 1;
        let mut index = hash as usize & mask;
        loop {
            let slot = self.buckets[index];
            let existing = StringRef::from_slot(slot)?;
            if existing.hash(cage) == hash && matches(existing) {
                return Some(Atom(slot));
            }
            index = (index + 1) & mask;
        }
    }

    fn insert(&mut self, cage: &Cage, string: StringRef) -> Atom {
        // Growing before inserting keeps the table from ever being full, which is what stops the
        // probe loop below from running forever.
        if (self.occupied + 1) * 4 > self.buckets.len() * 3 {
            self.grow(cage);
        }
        let hash = string.hash(cage);
        let mask = self.buckets.len() - 1;
        let mut index = hash as usize & mask;
        while self.buckets[index].is_pointer() {
            index = (index + 1) & mask;
        }
        string.mark_interned(cage);
        self.buckets[index] = string.slot();
        self.occupied += 1;
        Atom(string.slot())
    }

    fn grow(&mut self, cage: &Cage) {
        let capacity = if self.buckets.is_empty() {
            MIN_CAPACITY
        } else {
            self.buckets.len() * 2
        };
        let old = std::mem::replace(&mut self.buckets, vec![Slot::ZERO; capacity]);
        let mask = capacity - 1;
        for slot in old {
            let Some(string) = StringRef::from_slot(slot) else {
                continue;
            };
            // Every string in the table already has its hash cached, so rehashing reads a word
            // rather than walking the characters again.
            let mut index = string.hash(cage) as usize & mask;
            while self.buckets[index].is_pointer() {
                index = (index + 1) & mask;
            }
            self.buckets[index] = slot;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AtomTable, MIN_CAPACITY};
    use crate::bump::{BumpHeap, ObjectKind};
    use crate::string::StringRef;

    #[test]
    fn an_empty_table_costs_nothing() {
        let table = AtomTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.memory_bytes(), 0);
    }

    #[test]
    fn the_same_text_interns_to_the_same_atom() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        let first = table.intern(&mut heap, "length").unwrap();
        let second = table.intern(&mut heap, "length").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.slot(),
            second.slot(),
            "interning is only worth anything if the comparison is on the slot"
        );
        assert_eq!(table.len(), 1);
        assert_eq!(
            heap.census().totals(ObjectKind::String).count,
            1,
            "the second intern must not allocate a second copy"
        );
    }

    #[test]
    fn different_text_interns_to_different_atoms() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        let length = table.intern(&mut heap, "length").unwrap();
        let name = table.intern(&mut heap, "name").unwrap();
        assert_ne!(length, name);
        assert_eq!(table.len(), 2);
        assert_eq!(length.as_ascii(heap.cage()), Some("length"));
        assert_eq!(name.as_ascii(heap.cage()), Some("name"));
    }

    #[test]
    fn a_lookup_that_misses_allocates_nothing() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        table.intern(&mut heap, "constructor").unwrap();
        let before = heap.cursor();
        assert!(table.lookup(heap.cage(), "constructo").is_none());
        assert!(table.lookup(heap.cage(), "constructors").is_none());
        assert!(table.lookup(heap.cage(), "").is_none());
        assert!(table.lookup(heap.cage(), "constructor").is_some());
        assert_eq!(
            heap.cursor(),
            before,
            "a lookup that builds a candidate string is not a lookup"
        );
    }

    #[test]
    fn interning_marks_the_string_as_canonical() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        let loose = StringRef::from_str(&mut heap, "prototype").unwrap();
        assert!(!loose.is_interned(heap.cage()));
        let atom = table.intern_string(heap.cage(), loose);
        assert!(loose.is_interned(heap.cage()));
        assert_eq!(atom.as_string().slot(), loose.slot());

        // A second string with the same text collapses onto the first.
        let duplicate = StringRef::from_str(&mut heap, "prototype").unwrap();
        assert_ne!(duplicate.slot(), loose.slot());
        let same = table.intern_string(heap.cage(), duplicate);
        assert_eq!(same.slot(), loose.slot());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn interning_from_text_and_from_a_string_reach_the_same_atom() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        let from_text = table.intern(&mut heap, "valueOf").unwrap();
        let built = StringRef::from_str(&mut heap, "valueOf").unwrap();
        let from_string = table.intern_string(heap.cage(), built);
        assert_eq!(from_text, from_string);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn a_utf16_name_interns_like_any_other() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        let first = table.intern(&mut heap, "日本語").unwrap();
        let second = table.intern(&mut heap, "日本語").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.as_ascii(heap.cage()), None);
        assert_eq!(
            first.as_string().to_utf8(heap.cage()).unwrap(),
            "日本語",
            "the atom has to survive the round trip through the table"
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn the_table_grows_and_keeps_every_atom_findable() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        let names: Vec<String> = (0..500).map(|i| format!("property{i}")).collect();
        let atoms: Vec<_> = names
            .iter()
            .map(|name| table.intern(&mut heap, name).unwrap())
            .collect();

        assert_eq!(table.len(), 500);
        assert!(table.buckets.len() > MIN_CAPACITY, "the table has to grow");
        assert_eq!(table.atoms().len(), 500, "growth must not drop an atom");

        for (name, atom) in names.iter().zip(&atoms) {
            let found = table.lookup(heap.cage(), name).unwrap();
            assert_eq!(found, *atom, "{name} moved during a rehash");
        }

        // Interning them all again finds every one and allocates nothing.
        let before = heap.cursor();
        for (name, atom) in names.iter().zip(&atoms) {
            assert_eq!(table.intern(&mut heap, name).unwrap(), *atom);
        }
        assert_eq!(heap.cursor(), before);
    }

    #[test]
    fn the_table_stays_under_its_load_factor() {
        let mut heap = BumpHeap::new().unwrap();
        let mut table = AtomTable::new();
        for i in 0..100 {
            table.intern(&mut heap, &format!("n{i}")).unwrap();
            assert!(
                table.occupied * 4 <= table.buckets.len() * 3,
                "linear probing degrades badly past three quarters full"
            );
        }
        assert!(table.memory_bytes() >= table.buckets.len() * 4);
    }
}
