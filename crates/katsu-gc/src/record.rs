//! A fixed set of named values, which is what an object is until there are shapes.
//!
//! `console` has to exist before the object model does. It is a thing with a name on it that a
//! program reaches through `get_prop` and calls through `call_method`, and every one of those steps
//! needs something in the cage to point at. A record is that something: a count, then the names,
//! then the values, allocated in one go and never grown.
//!
//! It is deliberately not a small version of the real object. There is no prototype, no descriptor,
//! no enumeration order to defend, no `delete` and no way to add a property that was not there when
//! it was built. Those are all M1, and each one of them is a decision that a placeholder would get
//! wrong in a way somebody would then depend on. What a record does have is the one property
//! `spec/07-object-model.md` says every object in this heap has, which is a first word that says
//! what it is, so the day shapes arrive a record either grows one or stops existing.
//!
//! # Why the names and the values are two arrays
//!
//! The obvious layout is a name next to its value, and it is the wrong one twice over. A name is a
//! four byte slot and a value is eight, so pairing them either pads four bytes per entry or puts the
//! values on a four byte boundary. And a lookup only ever reads names until it finds the one it
//! wants, so keeping them adjacent means one cache line covers sixteen candidate names instead of
//! four.
//!
//! The lookup is a linear scan and that is not a placeholder either. Every name in it is interned,
//! so a comparison is four bytes against four bytes with no dereference, and a host object has a
//! handful of properties rather than a thousand. A hash would cost more to compute than the scan
//! costs to run at these sizes. The thing that eventually replaces it is an inline cache at the
//! `get_prop` site, not a better table here.

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};
use crate::object::{HeapKind, KIND_OFFSET, read_u32, read_u64, slot_of, write_u32, write_u64};
use crate::string::StringRef;

/// Bytes of header: the kind tag and the count.
const HEADER_SIZE: usize = 8;
/// How many entries this record holds.
const COUNT_OFFSET: usize = 4;
/// Bytes per name, which is one slot.
const NAME_SIZE: usize = 4;
/// Bytes per value, which is one boxed value rather than a slot, for the reason contexts give.
const VALUE_SIZE: usize = 8;

/// A host object: interned names and the values behind them, decided when it is built.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordRef(Slot);

impl std::fmt::Debug for RecordRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "record@{:?}", self.0)
    }
}

impl RecordRef {
    /// Build a record holding exactly `entries`, in the order given.
    ///
    /// Every name has to be interned by the caller, because the lookup compares addresses rather
    /// than text and a name that was not interned would be a property nothing can ever find. A name
    /// that appears twice keeps its first value and the second is unreachable, which is the caller's
    /// mistake to make rather than something worth a return type.
    ///
    /// Returns `None` if the heap is full or the record would be larger than the cage.
    #[must_use]
    pub fn new(heap: &mut BumpHeap, entries: &[(StringRef, u64)]) -> Option<RecordRef> {
        let count = u32::try_from(entries.len()).ok()?;
        let bytes = values_offset(count).checked_add(entries.len().checked_mul(VALUE_SIZE)?)?;
        let pointer = heap.allocate(bytes, ObjectKind::Object)?;
        let slot = slot_of(heap.cage(), pointer)?;
        // SAFETY: the allocation is `bytes` long, and every write below lands inside it because
        // both arrays are sized from the same count the header is about to record.
        unsafe {
            write_u32(pointer, KIND_OFFSET, HeapKind::Record.tag());
            write_u32(pointer, COUNT_OFFSET, count);
            for (index, (name, value)) in entries.iter().enumerate() {
                write_u32(
                    pointer,
                    HEADER_SIZE + index * NAME_SIZE,
                    name.slot().to_bits(),
                );
                write_u64(
                    pointer.as_ptr(),
                    values_offset(count) + index * VALUE_SIZE,
                    *value,
                );
            }
        }
        Some(RecordRef(slot))
    }

    /// How many names this record holds.
    #[must_use]
    pub fn len(self, cage: &Cage) -> u32 {
        // SAFETY: the slot points at a record, whose header is `HEADER_SIZE` bytes.
        unsafe { read_u32(cage, self.offset(), COUNT_OFFSET) }
    }

    /// Whether this record holds no names at all.
    #[must_use]
    pub fn is_empty(self, cage: &Cage) -> bool {
        self.len(cage) == 0
    }

    /// The value stored under `name`, or `None` if this record has no such name.
    ///
    /// The comparison is between slots and not between strings, which is only correct because both
    /// sides are interned. See the module documentation.
    #[must_use]
    pub fn get(self, cage: &Cage, name: StringRef) -> Option<u64> {
        self.index_of(cage, name)
            .and_then(|index| self.value_at(cage, index))
    }

    /// Replace the value stored under `name`, answering `false` if there is no such name.
    ///
    /// A record cannot grow, so this is an assignment to a property that already exists and nothing
    /// else. Refusing rather than adding is the honest answer while there is no shape to record a
    /// new property in.
    pub fn set(self, heap: &mut BumpHeap, name: StringRef, value: u64) -> bool {
        let Some(index) = self.index_of(heap.cage(), name) else {
            return false;
        };
        let count = self.len(heap.cage());
        let at = values_offset(count) + (index as usize) * VALUE_SIZE;
        // SAFETY: the index came from the scan over this record's own names, so it is inside the
        // count, and `&mut BumpHeap` means nothing else is reading the cage.
        unsafe {
            write_u64(heap.cage().address_of(self.offset()), at, value);
        }
        true
    }

    /// The name at `index`, or `None` if the index is past the end.
    ///
    /// Together with [`RecordRef::value_at`] this is how the collector will walk a record and how
    /// printing one reads it back, both of which want the entries in order rather than by name.
    #[must_use]
    pub fn name_at(self, cage: &Cage, index: u32) -> Option<StringRef> {
        if index >= self.len(cage) {
            return None;
        }
        // SAFETY: the index is inside the count the header records, so the name is inside the
        // allocation and was written when the record was built.
        let bits = unsafe {
            read_u32(
                cage,
                self.offset(),
                HEADER_SIZE + (index as usize) * NAME_SIZE,
            )
        };
        StringRef::from_slot(Slot::from_bits(bits))
    }

    /// The value at `index`, or `None` if the index is past the end.
    #[must_use]
    pub fn value_at(self, cage: &Cage, index: u32) -> Option<u64> {
        let count = self.len(cage);
        if index >= count {
            return None;
        }
        let at = values_offset(count) + (index as usize) * VALUE_SIZE;
        // SAFETY: as `name_at`.
        Some(unsafe { read_u64(cage.address_of(self.offset()), at) })
    }

    /// The slot this record lives at.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Read a record back out of a slot, or `None` if the slot is not a pointer.
    ///
    /// As with the function objects, the kind is not rechecked here, because the caller reached this
    /// through [`HeapKind::of`] and checking twice pays twice for one answer.
    #[must_use]
    pub fn from_slot(slot: Slot) -> Option<RecordRef> {
        slot.is_pointer().then_some(RecordRef(slot))
    }

    /// Where in the name array `name` sits, or `None` if it is not in it.
    fn index_of(self, cage: &Cage, name: StringRef) -> Option<u32> {
        let wanted = name.slot().to_bits();
        (0..self.len(cage)).find(|&index| {
            // SAFETY: the index is inside the count, so the read is inside the allocation.
            let bits = unsafe {
                read_u32(
                    cage,
                    self.offset(),
                    HEADER_SIZE + (index as usize) * NAME_SIZE,
                )
            };
            bits == wanted
        })
    }

    fn offset(self) -> u32 {
        self.0.as_offset().unwrap_or(0)
    }
}

/// Where the values start, which is after the names rounded up to eight bytes.
///
/// The rounding is the only padding a record has, it is at most four bytes for the whole object
/// rather than four per entry, and an odd number of names is what costs it.
const fn values_offset(count: u32) -> usize {
    let names = HEADER_SIZE + (count as usize) * NAME_SIZE;
    names.next_multiple_of(VALUE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::{HEADER_SIZE, RecordRef, VALUE_SIZE, values_offset};
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

    #[test]
    fn a_record_is_a_kind_of_its_own() {
        let mut heap = heap();
        let log = name(&mut heap, "log");
        let record = RecordRef::new(&mut heap, &[(log, 7)]).expect("should have room");
        assert_eq!(
            HeapKind::of(heap.cage(), record.slot()),
            Some(HeapKind::Record)
        );
    }

    #[test]
    fn a_name_that_was_put_in_comes_back_out() {
        let mut heap = heap();
        let log = name(&mut heap, "log");
        let error = name(&mut heap, "error");
        let record =
            RecordRef::new(&mut heap, &[(log, 11), (error, 22)]).expect("should have room");
        assert_eq!(record.len(heap.cage()), 2);
        assert_eq!(record.get(heap.cage(), log), Some(11));
        assert_eq!(record.get(heap.cage(), error), Some(22));
    }

    #[test]
    fn a_name_that_was_never_put_in_is_absent_rather_than_wrong() {
        let mut heap = heap();
        let log = name(&mut heap, "log");
        let missing = name(&mut heap, "table");
        let record = RecordRef::new(&mut heap, &[(log, 11)]).expect("should have room");
        assert_eq!(record.get(heap.cage(), missing), None);
    }

    #[test]
    fn two_strings_with_the_same_text_are_two_different_names() {
        // The scan compares addresses, so an uninterned lookup key misses even when the text
        // matches. This is the failure mode the module documentation warns about, pinned here so
        // that it is a documented rule rather than a surprise.
        let mut heap = heap();
        let interned = name(&mut heap, "log");
        let copy = name(&mut heap, "log");
        let record = RecordRef::new(&mut heap, &[(interned, 11)]).expect("should have room");
        assert_eq!(record.get(heap.cage(), interned), Some(11));
        assert_eq!(record.get(heap.cage(), copy), None);
    }

    #[test]
    fn the_entries_can_be_walked_in_order() {
        let mut heap = heap();
        let first = name(&mut heap, "log");
        let second = name(&mut heap, "error");
        let record =
            RecordRef::new(&mut heap, &[(first, 11), (second, 22)]).expect("should have room");
        assert_eq!(
            record.name_at(heap.cage(), 0).map(StringRef::slot),
            Some(first.slot())
        );
        assert_eq!(
            record.name_at(heap.cage(), 1).map(StringRef::slot),
            Some(second.slot())
        );
        assert_eq!(record.value_at(heap.cage(), 1), Some(22));
        assert_eq!(record.name_at(heap.cage(), 2), None);
        assert_eq!(record.value_at(heap.cage(), 2), None);
    }

    #[test]
    fn a_property_that_exists_can_be_written_and_one_that_does_not_cannot() {
        let mut heap = heap();
        let log = name(&mut heap, "log");
        let missing = name(&mut heap, "table");
        let record = RecordRef::new(&mut heap, &[(log, 11)]).expect("should have room");
        assert!(record.set(&mut heap, log, 33));
        assert_eq!(record.get(heap.cage(), log), Some(33));
        assert!(!record.set(&mut heap, missing, 44));
    }

    #[test]
    fn an_empty_record_is_legal_and_holds_nothing() {
        let mut heap = heap();
        let record = RecordRef::new(&mut heap, &[]).expect("should have room");
        assert!(record.is_empty(heap.cage()));
        assert_eq!(record.name_at(heap.cage(), 0), None);
    }

    #[test]
    fn a_record_wastes_at_most_four_bytes_and_only_on_an_odd_count() {
        // The layout claim from the module documentation, checked rather than asserted in prose.
        // Two names cost the header, two slots and two values with nothing left over.
        assert_eq!(values_offset(0), HEADER_SIZE);
        assert_eq!(values_offset(1), HEADER_SIZE + VALUE_SIZE);
        assert_eq!(values_offset(2), HEADER_SIZE + 2 * 4);
        assert_eq!(values_offset(3), HEADER_SIZE + 2 * VALUE_SIZE);

        let mut heap = heap();
        let first = name(&mut heap, "log");
        let second = name(&mut heap, "error");
        let before = heap.census().totals(ObjectKind::Object);
        RecordRef::new(&mut heap, &[(first, 11), (second, 22)]).expect("should have room");
        let after = heap.census().totals(ObjectKind::Object);
        assert_eq!(after.count - before.count, 1);
        assert_eq!(after.requested_bytes - before.requested_bytes, 32);
        assert_eq!(after.reserved_bytes - before.reserved_bytes, 32);
    }

    #[test]
    fn a_small_integer_is_not_a_record() {
        assert_eq!(RecordRef::from_slot(Slot::from_smi(3).unwrap()), None);
    }
}
