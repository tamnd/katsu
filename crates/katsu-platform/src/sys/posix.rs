//! The unix backend: mmap, mprotect, munmap.
//!
//! Covers Linux and macOS, which are the two tier 1 platforms in `spec/14-quality-bar.md`, and
//! would cover the BSDs unchanged if anybody asked.

use std::io;
use std::ptr::NonNull;

/// The page size the running kernel is using.
pub(crate) fn page_size() -> usize {
    // SAFETY: `sysconf` takes an integer name and returns a long, with no pointer arguments and no
    // memory effects. `_SC_PAGESIZE` is always supported.
    let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if raw > 0 {
        usize::try_from(raw).unwrap_or(4096)
    } else {
        4096
    }
}

/// Reserve `size` bytes of address space with the base aligned to `alignment`.
///
/// Alignment is achieved by over reserving by one alignment and unmapping the slack at both ends,
/// rather than by asking the kernel for it. There is a flag for this and it is spelled differently
/// on every platform, absent on some, and silently ignored on others, so the arithmetic is cheaper
/// than the portability.
///
/// `size` and `alignment` are already validated and page rounded by the caller.
pub(crate) fn reserve(size: usize, alignment: usize) -> io::Result<NonNull<u8>> {
    let padded = size
        .checked_add(alignment)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;

    // SAFETY: an anonymous private mapping with a null address hint asks the kernel to pick an
    // address, which is always valid to call. MAP_NORESERVE asks it not to charge the mapping
    // against commit accounting, which is what makes an eight gigabyte reservation reasonable on a
    // machine with four gigabytes of memory.
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            padded,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    let raw = raw.cast::<u8>();
    let start = raw as usize;
    let aligned = start.next_multiple_of(alignment);
    let head = aligned - start;
    let tail = padded - head - size;

    // SAFETY: `head` and `tail` are the slack at the two ends of a mapping we just made and still
    // own, so unmapping them cannot unmap anything that belongs to somebody else. A zero length
    // munmap is skipped because it is an error rather than a no-op.
    unsafe {
        if head > 0 {
            libc::munmap(raw.cast::<libc::c_void>(), head);
        }
        if tail > 0 {
            libc::munmap(raw.add(head + size).cast::<libc::c_void>(), tail);
        }
    }

    NonNull::new(aligned as *mut u8).ok_or_else(|| io::Error::from(io::ErrorKind::Other))
}

/// Give a whole reservation back to the kernel.
///
/// # Safety
///
/// `base` and `size` must be a reservation this module handed out, released exactly once.
pub(crate) unsafe fn release(base: NonNull<u8>, size: usize) {
    // SAFETY: the caller guarantees this range is a live reservation being released once.
    unsafe {
        libc::munmap(base.as_ptr().cast::<libc::c_void>(), size);
    }
}

/// Put readable and writable pages behind a page aligned range inside a reservation.
///
/// # Safety
///
/// The range must be page aligned and inside a live reservation.
pub(crate) unsafe fn commit(at: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: the caller guarantees the range is page aligned and owned by a live reservation, so
    // this can only change permissions on pages we already hold.
    let result = unsafe {
        libc::mprotect(
            at.cast::<libc::c_void>(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Drop the physical pages behind a range and make it unreadable again, keeping the address space.
///
/// This maps a fresh anonymous range over the old one rather than using `madvise`. The `madvise`
/// route is a hint, and on macOS `MADV_FREE` specifically means "you may take these pages when you
/// need them", so the old contents can still be there on the next read. A collector that recycled a
/// decommitted block would then see a dead object's bytes where it expects zeroes, and it would see
/// them only under memory pressure, which is the worst possible shape for a bug.
///
/// # Safety
///
/// The range must be page aligned and inside a live reservation.
pub(crate) unsafe fn decommit(at: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: the caller guarantees the range is page aligned and inside a reservation we own, so
    // MAP_FIXED here can only replace pages that already belong to us.
    let result = unsafe {
        libc::mmap(
            at.cast::<libc::c_void>(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if result == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
