//! Reserving address space, and committing pages inside it.
//!
//! The heap cage in `spec/07-object-model.md` 7.2 needs a large, aligned region of address
//! space that is reserved but not backed by anything, so that pointer compression can be a
//! truncation on the way in and an add on the way out. That is the primitive here.
//!
//! Reserving and committing are deliberately separate operations. A reservation costs address
//! space and nothing else, which is why an eight gigabyte reservation is reasonable on a machine
//! with four gigabytes of memory. Committing is what costs pages, and the memory budget in
//! `spec/02-the-10x-goal.md` counts committed pages, so it is the operation that has to be
//! explicit and countable.
//!
//! Nothing in this file names a system call. The four that exist live in `crate::sys`, once per
//! operating system, and everything here is bounds checking, page rounding and error shaping, which
//! is the same on every platform and is where the bugs actually are.

use std::io;
use std::ptr::NonNull;

use crate::{round_up_to_page, sys};

/// What went wrong reserving or committing address space.
#[derive(Debug, thiserror::Error)]
pub enum ReservationError {
    /// The alignment asked for was not a power of two, or was smaller than a page.
    #[error("alignment {0} is not a page aligned power of two")]
    BadAlignment(usize),
    /// A reservation of nothing was asked for.
    #[error("a reservation must be at least one page")]
    Empty,
    /// The kernel refused the mapping.
    #[error("could not reserve {size} bytes aligned to {alignment}: {source}")]
    Reserve {
        /// Bytes requested.
        size: usize,
        /// Alignment requested.
        alignment: usize,
        /// What the kernel said.
        source: io::Error,
    },
    /// A commit or decommit fell outside the reservation.
    #[error("the range {offset}..{offset}+{len} is outside a reservation of {size} bytes")]
    OutOfBounds {
        /// Start of the range, relative to the base.
        offset: usize,
        /// Length of the range.
        len: usize,
        /// Size of the reservation.
        size: usize,
    },
    /// The kernel refused a permission change.
    #[error("could not change protection on {len} bytes at offset {offset}: {source}")]
    Protect {
        /// Start of the range, relative to the base.
        offset: usize,
        /// Length of the range.
        len: usize,
        /// What the kernel said.
        source: io::Error,
    },
}

/// A region of address space that is reserved but not readable, writable or executable.
///
/// Dropping it returns the whole region to the operating system. Nothing hands out references
/// into a reservation, because the reservation does not know what is stored in it, so the
/// pointer arithmetic lives with whoever does know.
#[derive(Debug)]
pub struct Reservation {
    base: NonNull<u8>,
    size: usize,
}

// SAFETY: a reservation owns a range of address space and nothing else. It has no interior
// mutability and no thread affinity, and the kernel calls it makes are all thread safe, so
// moving one between threads and sharing one across threads are both sound. Whether the bytes
// inside are safe to share concurrently is the caller's problem and is not claimed here.
unsafe impl Send for Reservation {}
// SAFETY: as above.
unsafe impl Sync for Reservation {}

impl Reservation {
    /// Reserve `size` bytes of address space, with the base aligned to `alignment`.
    ///
    /// Nothing is committed. Every page starts unreadable, so a stray access into a reservation
    /// faults rather than silently reading zeroes, which is the whole reason the cage reserves
    /// guard regions it never intends to use.
    ///
    /// How the base comes out aligned is the backend's business. Unix over reserves and trims the
    /// ends, Windows asks for the alignment directly. Both are asked for the same thing and both
    /// return a base that is at least as aligned as requested.
    ///
    /// # Errors
    ///
    /// Returns [`ReservationError::BadAlignment`] if the alignment is not a page aligned power
    /// of two, and [`ReservationError::Reserve`] if the kernel refuses the mapping.
    pub fn reserve(size: usize, alignment: usize) -> Result<Reservation, ReservationError> {
        let page = crate::page_size();
        if !alignment.is_power_of_two() || alignment < page {
            return Err(ReservationError::BadAlignment(alignment));
        }
        let size = round_up_to_page(size);
        if size == 0 {
            return Err(ReservationError::Empty);
        }

        let base = sys::reserve(size, alignment).map_err(|source| ReservationError::Reserve {
            size,
            alignment,
            source,
        })?;
        Ok(Reservation { base, size })
    }

    /// The first byte of the reservation.
    #[must_use]
    pub const fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// The size of the reservation in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.size
    }

    /// Whether the reservation is empty, which it never is.
    ///
    /// Present because clippy asks for it next to `len`. A zero sized reservation is refused at
    /// construction, so this always returns false, and it is here so that the absence of it does
    /// not read as an oversight.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Make `len` bytes at `offset` readable and writable.
    ///
    /// The range is rounded out to whole pages, because permissions have page granularity and
    /// pretending otherwise would silently commit a neighbouring page.
    ///
    /// # Errors
    ///
    /// Returns [`ReservationError::OutOfBounds`] if the range leaves the reservation, and
    /// [`ReservationError::Protect`] if the kernel refuses.
    pub fn commit(&self, offset: usize, len: usize) -> Result<(), ReservationError> {
        let (offset, len) = self.page_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        // SAFETY: the range was checked against the reservation and rounded to whole pages by
        // `page_range`, and the reservation owns every page in it until it is dropped.
        unsafe { sys::commit(self.base.as_ptr().add(offset), len) }.map_err(|source| {
            ReservationError::Protect {
                offset,
                len,
                source,
            }
        })
    }

    /// Give `len` bytes at `offset` back to the operating system without releasing the address
    /// space, and make them unreadable again.
    ///
    /// This is the operation behind the promise in `spec/08-gc-and-memory.md` 8.7 that memory is
    /// returned rather than merely marked free. A runtime that grows to two hundred megabytes
    /// during startup and never gives it back has failed the memory goal whatever its own heap
    /// accounting says.
    ///
    /// Whatever the backend does, a page committed again after this reads as zero rather than
    /// giving back what was in it. That is not free on either platform and the note in each backend
    /// says what it costs, but a collector that recycles a decommitted block must never see a dead
    /// object's bytes, so it is not negotiable.
    ///
    /// # Errors
    ///
    /// Returns [`ReservationError::OutOfBounds`] if the range leaves the reservation, and
    /// [`ReservationError::Protect`] if the kernel refuses the mapping.
    pub fn decommit(&self, offset: usize, len: usize) -> Result<(), ReservationError> {
        let (offset, len) = self.page_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        // SAFETY: the range was checked against the reservation and rounded to whole pages by
        // `page_range`, and the reservation owns every page in it until it is dropped.
        unsafe { sys::decommit(self.base.as_ptr().add(offset), len) }.map_err(|source| {
            ReservationError::Protect {
                offset,
                len,
                source,
            }
        })
    }

    /// Round a range out to whole pages and check that it is inside the reservation.
    fn page_range(&self, offset: usize, len: usize) -> Result<(usize, usize), ReservationError> {
        let end = offset
            .checked_add(len)
            .ok_or(ReservationError::OutOfBounds {
                offset,
                len,
                size: self.size,
            })?;
        if end > self.size {
            return Err(ReservationError::OutOfBounds {
                offset,
                len,
                size: self.size,
            });
        }
        let page = crate::page_size();
        let start = offset - (offset % page);
        let end = round_up_to_page(end).min(self.size);
        Ok((start, end - start))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // SAFETY: this reservation owns exactly this range, nothing hands out pointers into it
        // that outlive it, and it is released once because a Reservation cannot be duplicated.
        unsafe {
            sys::release(self.base, self.size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Reservation, ReservationError};
    use crate::page_size;

    const ONE_MIB: usize = 1 << 20;

    #[test]
    fn a_reservation_lands_on_the_alignment_it_asked_for() {
        for shift in 12..=26 {
            let alignment = 1usize << shift;
            if alignment < page_size() {
                continue;
            }
            let reservation = Reservation::reserve(ONE_MIB, alignment).unwrap();
            assert_eq!(
                reservation.base() as usize % alignment,
                0,
                "base {:p} is not aligned to {alignment}",
                reservation.base()
            );
            assert!(reservation.len() >= ONE_MIB);
        }
    }

    #[test]
    fn reserving_address_space_does_not_cost_memory() {
        // Four gigabytes, which is the cage size, on whatever machine is running the tests.
        // If this ever starts failing for lack of memory then the reservation is committing,
        // which is the bug this test exists to catch.
        let cage = 4 * 1024 * ONE_MIB;
        let reservation = Reservation::reserve(cage, page_size()).unwrap();
        assert_eq!(reservation.len(), cage);
    }

    #[test]
    fn committed_pages_are_writable_and_read_back_as_zero() {
        let reservation = Reservation::reserve(ONE_MIB, page_size()).unwrap();
        reservation.commit(0, ONE_MIB).unwrap();

        // SAFETY: the whole reservation was just committed, so every byte in it is mapped
        // readable and writable, and nothing else holds a pointer into it.
        unsafe {
            let base = reservation.base();
            for offset in (0..ONE_MIB).step_by(4096) {
                assert_eq!(base.add(offset).read(), 0, "fresh pages should be zero");
                base.add(offset).write(0xAB);
                assert_eq!(base.add(offset).read(), 0xAB);
            }
        }
    }

    #[test]
    fn a_range_that_leaves_the_reservation_is_refused_rather_than_clamped() {
        let reservation = Reservation::reserve(ONE_MIB, page_size()).unwrap();
        assert!(matches!(
            reservation.commit(ONE_MIB, 1),
            Err(ReservationError::OutOfBounds { .. })
        ));
        assert!(matches!(
            reservation.commit(0, ONE_MIB + 1),
            Err(ReservationError::OutOfBounds { .. })
        ));
        assert!(matches!(
            reservation.commit(usize::MAX, 1),
            Err(ReservationError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn alignment_has_to_be_a_page_aligned_power_of_two() {
        assert!(matches!(
            Reservation::reserve(ONE_MIB, 3),
            Err(ReservationError::BadAlignment(3))
        ));
        assert!(matches!(
            Reservation::reserve(ONE_MIB, 0),
            Err(ReservationError::BadAlignment(0))
        ));
        // A power of two smaller than a page is refused too, because rounding it up silently
        // would give back a reservation that does not meet the contract it was asked for.
        assert!(matches!(
            Reservation::reserve(ONE_MIB, 8),
            Err(ReservationError::BadAlignment(8))
        ));
    }

    #[test]
    fn a_reservation_of_nothing_is_refused() {
        assert!(matches!(
            Reservation::reserve(0, page_size()),
            Err(ReservationError::Empty)
        ));
    }

    #[test]
    fn decommit_then_commit_gives_back_zeroed_pages() {
        let reservation = Reservation::reserve(ONE_MIB, page_size()).unwrap();
        reservation.commit(0, ONE_MIB).unwrap();

        // SAFETY: the range is committed for the duration of this write.
        unsafe { reservation.base().write(0x7F) };

        reservation.decommit(0, ONE_MIB).unwrap();
        reservation.commit(0, ONE_MIB).unwrap();

        // SAFETY: recommitted, so readable again.
        let recycled = unsafe { reservation.base().read() };
        assert_eq!(
            recycled, 0,
            "a recommitted page must not leak its old contents"
        );
    }

    #[test]
    fn a_reservation_can_be_moved_to_another_thread() {
        let reservation = Reservation::reserve(ONE_MIB, page_size()).unwrap();
        let size = std::thread::spawn(move || {
            reservation.commit(0, 4096).unwrap();
            reservation.len()
        })
        .join()
        .unwrap();
        assert_eq!(size, ONE_MIB);
    }
}
