//! The heap cage, and the compressed slots that live inside it.
//!
//! `spec/07-object-model.md` 7.2 puts the whole JavaScript heap for an isolate inside one
//! reserved region of address space. That buys two things at once, which is why it is a day one
//! decision rather than an optimisation.
//!
//! It buys the memory goal. A heap slot becomes four bytes instead of eight, and tagged values
//! are around seventy percent of a real heap, which is where V8's measured heap reduction of up
//! to forty three percent came from. There is no version of a 10x memory target that does not
//! include this.
//!
//! It buys a security boundary. The threat model in `spec/01-research-2026.md` 1.7 is an attacker
//! turning a JIT type confusion into a corrupted pointer and then into arbitrary process read and
//! write. A corrupted reference inside a cage is a thirty two bit offset, so the worst it can
//! reach is somewhere else inside the same cage, which is a far weaker primitive than an
//! arbitrary address.
//!
//! The cost is that an isolate's heap is capped at four gigabytes. That is the trade Chrome made
//! and it is a product limitation that belongs in the documentation rather than being discovered
//! by somebody with a six gigabyte working set.

use std::fmt;

use katsu_platform::{Reservation, ReservationError};

/// The size of the cage, and therefore the largest heap a single isolate can have.
///
/// Four gigabytes, because a compressed slot has thirty two bits and one of them is the tag, so
/// a slot addresses two gigabytes of eight byte aligned objects, and the cage is sized so that
/// every offset a slot can name is inside it.
pub const CAGE_SIZE: usize = 4 << 30;

/// Unreadable address space reserved immediately above the cage.
///
/// Indexing arithmetic on a typed array is a thirty two bit offset added to a base inside the
/// cage, so an out of bounds index can land at most four gigabytes past the end. Reserving that
/// range and never mapping it turns those accesses into a fault instead of a read of whatever
/// the allocator happened to put there. Cloudflare's Workers hardening describes the same shape.
pub const GUARD_SIZE: usize = 4 << 30;

/// The alignment every object in the cage gets.
///
/// Eight bytes, which leaves the low three bits of an offset free and is what lets a compressed
/// pointer carry its tag in bit zero.
pub const OBJECT_ALIGN: usize = 8;

/// Why a cage could not be created.
#[derive(Debug, thiserror::Error)]
pub enum CageError {
    /// The operating system would not give us the address space.
    ///
    /// Eight gigabytes of reservation on a machine with far less memory is normally fine, since
    /// nothing is committed, but a process under a virtual address space rlimit will fail here.
    #[error("could not reserve the {CAGE_SIZE} byte cage: {0}")]
    Reserve(#[from] ReservationError),
}

/// One isolate's reserved heap region.
///
/// The base is aligned to [`CAGE_SIZE`], which is the property the whole scheme rests on: the low
/// thirty two bits of the base are zero, so decompressing an offset is a bitwise or and
/// compressing an address is a truncation, with no masking and no comparison on either path.
pub struct Cage {
    reservation: Reservation,
    base: *mut u8,
}

// SAFETY: a cage owns its reservation and holds a derived base pointer into it. Neither has
// interior mutability or thread affinity, and the reservation is itself Send and Sync for the
// same reason. Whether the objects inside are safe to share is the heap's problem, not the
// cage's, and the heap is not Sync.
unsafe impl Send for Cage {}

// The reservation is deliberately left out. It prints as a base and a size that are the same
// two numbers already on the line, and a Debug that repeats itself is a Debug people stop reading.
#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for Cage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cage")
            .field("base", &self.base)
            .field("size", &CAGE_SIZE)
            .field("guard", &GUARD_SIZE)
            .finish()
    }
}

impl Cage {
    /// Reserve a cage, with its guard region above it.
    ///
    /// Nothing is committed, so this costs address space and no memory. The reservation is the
    /// cage plus the guard, and the alignment request is what forces the low thirty two bits of
    /// the base to zero.
    ///
    /// # Errors
    ///
    /// Returns [`CageError::Reserve`] if the operating system refuses the reservation.
    pub fn new() -> Result<Cage, CageError> {
        let reservation = Reservation::reserve(CAGE_SIZE + GUARD_SIZE, CAGE_SIZE)?;
        let base = reservation.base();
        Ok(Cage { reservation, base })
    }

    /// The first byte of the cage.
    ///
    /// This is the value that gets pinned into a register, per spec 07.2, so that decompression
    /// in generated code does not have to load it.
    #[must_use]
    pub const fn base(&self) -> *mut u8 {
        self.base
    }

    /// Make the cage readable and writable from `from` up to `to`.
    ///
    /// Rounded out to whole pages by the reservation. Committing from the bottom rather than in
    /// arbitrary places keeps the committed set one contiguous range, which is what makes the
    /// number reported to the memory budget a single subtraction.
    ///
    /// The caller passes the range it is adding rather than the new high water mark, so that a
    /// heap growing one chunk at a time asks for one chunk each time. Asking for the whole
    /// committed region on every growth is quadratic, and it is quadratic on every platform. Linux
    /// hides it, because `mprotect` over a range that already has the permissions it is being given
    /// is close to free, and Windows does not, because `VirtualAlloc` with `MEM_COMMIT` walks every
    /// page in the range whether or not it is already committed. The Windows number is the honest
    /// one and it is what turned this up.
    ///
    /// # Errors
    ///
    /// Returns [`CageError::Reserve`] if the range leaves the cage or the kernel refuses.
    pub fn commit_range(&self, from: usize, to: usize) -> Result<(), CageError> {
        if to > CAGE_SIZE || from > to {
            return Err(CageError::Reserve(ReservationError::OutOfBounds {
                offset: from,
                len: to.saturating_sub(from),
                size: CAGE_SIZE,
            }));
        }
        self.reservation.commit(from, to - from)?;
        Ok(())
    }

    /// Give the range from `bytes` to the end of the committed region back to the kernel.
    ///
    /// # Errors
    ///
    /// Returns [`CageError::Reserve`] if the range leaves the cage or the kernel refuses.
    pub fn decommit_from(&self, bytes: usize, committed: usize) -> Result<(), CageError> {
        if committed > CAGE_SIZE || bytes > committed {
            return Err(CageError::Reserve(ReservationError::OutOfBounds {
                offset: bytes,
                len: committed.saturating_sub(bytes),
                size: CAGE_SIZE,
            }));
        }
        self.reservation.decommit(bytes, committed - bytes)?;
        Ok(())
    }

    /// Whether `address` points inside the cage.
    ///
    /// The guard region is deliberately not inside. An address in the guard is a bug that we
    /// want to see as a bug, not as a valid pointer that happens to fault later.
    #[must_use]
    pub fn contains(&self, address: *const u8) -> bool {
        let base = self.base as usize;
        let address = address as usize;
        address >= base && address < base + CAGE_SIZE
    }

    /// Turn an address inside the cage into an offset from the base.
    ///
    /// Returns `None` for an address outside the cage rather than producing an offset that would
    /// decompress to somewhere else, because a silently wrong pointer is the failure mode the
    /// cage exists to prevent.
    #[must_use]
    pub fn offset_of(&self, address: *const u8) -> Option<u32> {
        if !self.contains(address) {
            return None;
        }
        // The subtraction fits in u32 because the cage is exactly 2^32 bytes and the address was
        // just checked to be inside it.
        u32::try_from(address as usize - self.base as usize).ok()
    }

    /// Turn an offset from the base back into an address.
    ///
    /// This is the hot direction, run on every property load, and it is a single bitwise or
    /// because the base is aligned to the cage size. Every `u32` is a valid offset, which is why
    /// this returns an address rather than an option.
    #[must_use]
    pub fn address_of(&self, offset: u32) -> *mut u8 {
        // Or rather than add, to make it obvious that the two halves cannot interfere and that
        // no carry is possible. A wrong offset lands somewhere else in the cage, by construction.
        ((self.base as usize) | offset as usize) as *mut u8
    }
}

/// A thirty two bit value as it is stored in a heap slot.
///
/// Object property slots, array elements and context slots are four bytes each, per spec 07.1.
/// This is the other half of the pair whose sixty four bit register form is `katsu_vm::Value`.
///
/// The tag is bit zero. A zero there means the rest is a thirty one bit signed integer, which
/// JavaScript engines have called a Smi since SELF and which covers the overwhelming majority of
/// numbers in real programs. A one there means the rest is an offset into the cage, which works
/// because objects are eight byte aligned so the low three bits of a real offset are always zero.
///
/// A consequence worth knowing: a slot of all zero bits is the integer zero. Freshly committed
/// pages read as zero, so an uninitialised slot is a valid number rather than a trap
/// representation, and nothing has to walk a new block to fill it in.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(u32);

/// The most negative integer a slot can hold inline.
pub const SMI_MIN: i32 = -(1 << 30);
/// The largest integer a slot can hold inline.
pub const SMI_MAX: i32 = (1 << 30) - 1;

/// The bit that says a slot holds a pointer rather than an integer.
const POINTER_TAG: u32 = 1;

impl Slot {
    /// The integer zero, which is also what a freshly committed page is full of.
    pub const ZERO: Slot = Slot(0);

    /// Store an integer inline, if it fits in thirty one bits.
    ///
    /// Returns `None` outside [`SMI_MIN`] and [`SMI_MAX`]. The caller boxes it as a heap number
    /// in that case, which is the same thing every engine does and the reason `array.length`
    /// stops being cheap somewhere north of a billion.
    #[must_use]
    pub const fn from_smi(n: i32) -> Option<Slot> {
        if n < SMI_MIN || n > SMI_MAX {
            return None;
        }
        // The shift is on the unsigned reinterpretation so that a negative number's sign bit
        // shifts out of the top rather than being undefined behaviour on overflow.
        Some(Slot(n.cast_unsigned() << 1))
    }

    /// Store an offset into the cage.
    ///
    /// # Panics
    ///
    /// Panics if the offset is not eight byte aligned. Every object in the cage is aligned by
    /// the allocator, so an unaligned offset here is a bug in the allocator or a fabricated
    /// value, and either one is worth stopping for rather than storing a slot that decompresses
    /// into the middle of an object.
    #[must_use]
    pub const fn from_offset(offset: u32) -> Slot {
        assert!(
            (offset as usize).is_multiple_of(OBJECT_ALIGN),
            "a cage offset must be eight byte aligned"
        );
        Slot(offset | POINTER_TAG)
    }

    /// Whether this slot holds an inline integer.
    #[must_use]
    pub const fn is_smi(self) -> bool {
        self.0 & POINTER_TAG == 0
    }

    /// Whether this slot holds an offset into the cage.
    #[must_use]
    pub const fn is_pointer(self) -> bool {
        self.0 & POINTER_TAG != 0
    }

    /// The inline integer, if this slot holds one.
    #[must_use]
    pub const fn as_smi(self) -> Option<i32> {
        if self.is_smi() {
            // An arithmetic shift, so the sign comes back. This is why the tag is in the low bit
            // rather than the high one.
            Some(self.0.cast_signed() >> 1)
        } else {
            None
        }
    }

    /// The cage offset, if this slot holds one.
    #[must_use]
    pub const fn as_offset(self) -> Option<u32> {
        if self.is_pointer() {
            Some(self.0 & !POINTER_TAG)
        } else {
            None
        }
    }

    /// The raw thirty two bits, for writing into a slot.
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    /// Rebuild a slot from raw bits read out of the heap.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Slot {
        Slot(bits)
    }
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_smi() {
            Some(n) => write!(f, "{n}smi"),
            None => write!(f, "cage+{:#x}", self.0 & !POINTER_TAG),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CAGE_SIZE, Cage, GUARD_SIZE, OBJECT_ALIGN, SMI_MAX, SMI_MIN, Slot};

    #[test]
    fn the_cage_base_has_a_clear_low_half() {
        let cage = Cage::new().unwrap();
        assert_eq!(
            cage.base() as usize & 0xFFFF_FFFF,
            0,
            "the whole compression scheme depends on this, so it is checked rather than assumed"
        );
    }

    #[test]
    fn an_offset_survives_a_round_trip_through_an_address() {
        let cage = Cage::new().unwrap();
        // The two ends, a middle, and the last representable offset. The last one is the
        // interesting case: it is what an offset looks like immediately before it would need a
        // thirty third bit.
        let align = u32::try_from(OBJECT_ALIGN).unwrap();
        for offset in [0u32, align, 1 << 16, 1 << 31, u32::MAX - (align - 1)] {
            let address = cage.address_of(offset);
            assert!(cage.contains(address), "offset {offset} left the cage");
            assert_eq!(cage.offset_of(address), Some(offset));
        }
    }

    #[test]
    fn an_address_outside_the_cage_has_no_offset() {
        let cage = Cage::new().unwrap();
        let stack = &0u8 as *const u8;
        assert_eq!(
            cage.offset_of(stack),
            None,
            "a stack address must not compress, or the cage is not a boundary"
        );

        // One past the end, which is the first byte of the guard region.
        let just_past = (cage.base() as usize + CAGE_SIZE) as *const u8;
        assert!(!cage.contains(just_past));
        assert_eq!(cage.offset_of(just_past), None);
    }

    #[test]
    fn the_guard_region_is_reserved_and_stays_unreadable() {
        let cage = Cage::new().unwrap();
        // Committing has to stay inside the cage. If this ever succeeded, the guard would be
        // writable memory rather than a trap, and an out of bounds typed array index would read
        // real data instead of faulting.
        assert!(cage.commit_range(0, CAGE_SIZE + 1).is_err());
        assert!(cage.commit_range(0, CAGE_SIZE + GUARD_SIZE).is_err());
    }

    #[test]
    fn a_commit_range_that_runs_backwards_is_refused() {
        let cage = Cage::new().unwrap();
        // A caller that subtracts in the wrong order gets an error rather than a range that
        // wraps and commits most of the address space, which is the failure this guards.
        assert!(cage.commit_range(64 * 1024, 0).is_err());
        assert!(cage.commit_range(128 * 1024, 64 * 1024).is_err());
        // An empty range at a legal place is not an error, because a heap that has grown to
        // exactly a chunk boundary asks for nothing and should not have to check first.
        assert!(cage.commit_range(64 * 1024, 64 * 1024).is_ok());
    }

    #[test]
    fn growing_a_chunk_at_a_time_only_commits_the_new_chunk() {
        let cage = Cage::new().unwrap();
        // Each call adds one chunk rather than recommitting everything below it. What is being
        // checked is that the earlier chunks stay committed and keep their contents, because a
        // range based commit that got its arithmetic wrong would either fault here or zero what
        // was already written.
        for chunk in 0..8 {
            let from = chunk * 64 * 1024;
            let to = from + 64 * 1024;
            cage.commit_range(from, to).unwrap();

            // SAFETY: everything up to `to` has been committed by this loop, and this test owns
            // the cage, so nothing else holds a pointer into it.
            unsafe {
                let byte = cage.base().add(from);
                assert_eq!(byte.read(), 0, "a freshly committed page must read as zero");
                byte.write(0xC5);
            }
        }

        // SAFETY: as above, and every offset touched here was committed and written in the loop.
        unsafe {
            for chunk in 0..8 {
                assert_eq!(
                    cage.base().add(chunk * 64 * 1024).read(),
                    0xC5,
                    "committing a later chunk must not disturb an earlier one"
                );
            }
        }
    }

    #[test]
    fn committing_makes_the_bottom_of_the_cage_usable() {
        let cage = Cage::new().unwrap();
        cage.commit_range(0, 64 * 1024).unwrap();

        // SAFETY: the first 64 KiB were just committed, so they are mapped readable and writable,
        // and this test owns the cage.
        unsafe {
            let base = cage.base();
            assert_eq!(base.read(), 0);
            base.write(0x5A);
            assert_eq!(base.read(), 0x5A);
            base.add(64 * 1024 - 1).write(0x5A);
        }
    }

    #[test]
    fn a_zero_slot_is_the_integer_zero() {
        // Freshly committed pages are zero, so this is what an uninitialised slot reads as, and
        // it needs to be a valid value rather than a trap representation.
        assert_eq!(Slot::from_bits(0).as_smi(), Some(0));
        assert_eq!(Slot::ZERO, Slot::from_smi(0).unwrap());
    }

    #[test]
    fn every_representable_smi_round_trips() {
        // The boundaries, and then a prime stride across the range so the walk is not aligned
        // with any power of two.
        let mut cases = vec![SMI_MIN, SMI_MIN + 1, -1, 0, 1, SMI_MAX - 1, SMI_MAX];
        let mut n = SMI_MIN;
        while n < SMI_MAX - 7_919_191 {
            n += 7_919_191;
            cases.push(n);
        }
        for n in cases {
            let slot = Slot::from_smi(n).unwrap_or_else(|| panic!("{n} should fit in a slot"));
            assert!(slot.is_smi());
            assert!(!slot.is_pointer());
            assert_eq!(slot.as_smi(), Some(n));
            assert_eq!(slot.as_offset(), None);
        }
    }

    #[test]
    fn an_integer_too_large_for_a_slot_is_refused_rather_than_truncated() {
        assert_eq!(Slot::from_smi(SMI_MAX + 1), None);
        assert_eq!(Slot::from_smi(SMI_MIN - 1), None);
        assert_eq!(Slot::from_smi(i32::MAX), None);
        assert_eq!(Slot::from_smi(i32::MIN), None);
    }

    #[test]
    fn a_pointer_slot_never_looks_like_an_integer() {
        for offset in (0..1 << 20).step_by(OBJECT_ALIGN) {
            let slot = Slot::from_offset(offset);
            assert!(slot.is_pointer(), "offset {offset} lost its tag");
            assert_eq!(slot.as_offset(), Some(offset));
            assert_eq!(slot.as_smi(), None);
        }
    }

    #[test]
    #[should_panic(expected = "eight byte aligned")]
    fn an_unaligned_offset_is_a_bug_and_says_so() {
        let _ = Slot::from_offset(4);
    }

    #[test]
    fn debug_output_says_which_half_of_the_scheme_a_slot_is_in() {
        assert_eq!(format!("{:?}", Slot::from_smi(-7).unwrap()), "-7smi");
        assert_eq!(format!("{:?}", Slot::from_offset(0x20)), "cage+0x20");
    }
}
