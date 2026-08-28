//! The Windows backend: VirtualAlloc2, VirtualAlloc, VirtualFree.
//!
//! Windows already splits reserving from committing, which is where the vocabulary in this crate
//! came from in the first place, so the mapping is closer than it is on unix. The differences that
//! matter are all about granularity and about what you are allowed to free.
//!
//! Windows has two granularities, not one. Pages are 4 KiB and permissions work in pages, but
//! reservations are placed on a 64 KiB allocation granularity boundary. `page_size` reports the
//! page, because that is what the memory budget in `spec/02-the-10x-goal.md` counts and what
//! rounding a commit range means, and the coarser number is only used where it has to be, which is
//! when asking for an aligned base.
//!
//! You cannot give back part of a reservation. `VirtualFree` with `MEM_RELEASE` frees exactly the
//! region a matching `VirtualAlloc` returned and nothing smaller, so the over reserve and trim the
//! ends trick the unix backend uses is not available. `VirtualAlloc2` takes an alignment directly
//! instead, which is both simpler and one call rather than three. It needs Windows 10 1803 or
//! Server 2019, which is not a constraint worth engineering around in 2026.

use std::io;
use std::mem::MaybeUninit;
use std::ptr::{NonNull, null, null_mut};

use windows_sys::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_DECOMMIT, MEM_EXTENDED_PARAMETER,
    MEM_EXTENDED_PARAMETER_0, MEM_EXTENDED_PARAMETER_1, MEM_RELEASE, MEM_RESERVE,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
    VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

/// Ask the kernel about its two granularities.
///
/// One call rather than two, because `GetSystemInfo` fills a struct and both numbers are in it.
fn system_info() -> (usize, usize) {
    let mut info = MaybeUninit::<SYSTEM_INFO>::uninit();
    // SAFETY: `GetSystemInfo` writes a whole `SYSTEM_INFO` through the pointer it is given and
    // reads nothing, so an uninitialised one is the documented way to call it. It cannot fail.
    let info = unsafe {
        GetSystemInfo(info.as_mut_ptr());
        info.assume_init()
    };
    let page = usize::try_from(info.dwPageSize).unwrap_or(4096);
    let granularity = usize::try_from(info.dwAllocationGranularity).unwrap_or(64 * 1024);
    (page.max(4096), granularity.max(page))
}

/// The page size the running kernel is using.
pub(crate) fn page_size() -> usize {
    system_info().0
}

/// Reserve `size` bytes of address space with the base aligned to `alignment`.
///
/// The alignment actually asked for is the larger of what the caller wanted and the allocation
/// granularity, because `VirtualAlloc2` refuses anything finer. Rounding up still satisfies the
/// caller, since a base aligned to 64 KiB is aligned to 4 KiB as well, and the cage in
/// `spec/07-object-model.md` asks for four gigabytes anyway, so this only ever matters in tests.
///
/// `size` and `alignment` are already validated and page rounded by the caller.
pub(crate) fn reserve(size: usize, alignment: usize) -> io::Result<NonNull<u8>> {
    let (_, granularity) = system_info();

    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        // Null at both ends means no constraint on where the region lands, which is what we want.
        // The only requirement is the alignment.
        LowestStartingAddress: null_mut(),
        HighestEndingAddress: null_mut(),
        Alignment: alignment.max(granularity),
    };

    // The type tag lives in the low eight bits of a bitfield and the rest of that word is reserved,
    // so setting it means writing the whole word with the reserved bits left at zero. The tag is a
    // fixed positive constant from the header, which is why the conversion cannot fail.
    let mut parameter = MEM_EXTENDED_PARAMETER {
        Anonymous1: MEM_EXTENDED_PARAMETER_0 {
            _bitfield: u64::try_from(MemExtendedParameterAddressRequirements)
                .expect("the address requirements tag is a small positive constant"),
        },
        Anonymous2: MEM_EXTENDED_PARAMETER_1 {
            Pointer: std::ptr::from_mut(&mut requirements).cast(),
        },
    };

    // SAFETY: a null base with MEM_RESERVE asks the kernel to pick an address, which is always
    // valid to call. The extended parameter array is one element long and `requirements` outlives
    // the call. PAGE_NOACCESS is what makes this a reservation rather than a commitment: no pages
    // are charged and any access faults.
    let raw = unsafe {
        VirtualAlloc2(
            // Null process handle means this process.
            null_mut(),
            null(),
            size,
            MEM_RESERVE,
            PAGE_NOACCESS,
            std::ptr::from_mut(&mut parameter),
            1,
        )
    };

    NonNull::new(raw.cast::<u8>()).ok_or_else(io::Error::last_os_error)
}

/// Give a whole reservation back to the kernel.
///
/// `MEM_RELEASE` requires a size of zero and frees exactly the region the matching reservation
/// returned, which is why nothing here tries to release a part of one.
///
/// # Safety
///
/// `base` must be a reservation this module handed out, released exactly once.
pub(crate) unsafe fn release(base: NonNull<u8>, _size: usize) {
    // SAFETY: the caller guarantees this is a live reservation being released once, and MEM_RELEASE
    // with a zero size is the documented way to free the whole of one.
    unsafe {
        VirtualFree(base.as_ptr().cast(), 0, MEM_RELEASE);
    }
}

/// Put readable and writable pages behind a page aligned range inside a reservation.
///
/// Committing a range that is already committed succeeds and changes nothing, which the bump heap
/// above relies on rather than tracking which pages it has already asked for.
///
/// # Safety
///
/// The range must be page aligned and inside a live reservation.
pub(crate) unsafe fn commit(at: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: the caller guarantees the range is page aligned and inside a reservation we own, so
    // this can only commit pages within address space we already hold.
    let raw = unsafe { VirtualAlloc(at.cast(), len, MEM_COMMIT, PAGE_READWRITE) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drop the physical pages behind a range and make it unreadable again, keeping the address space.
///
/// `MEM_DECOMMIT` is exactly this operation, so unlike the unix backend there is no remapping trick
/// here and no ambiguity about whether the old contents can come back. A page committed again after
/// this reads as zero, which the layers above depend on.
///
/// # Safety
///
/// The range must be page aligned and inside a live reservation.
pub(crate) unsafe fn decommit(at: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: the caller guarantees the range is page aligned and inside a reservation we own, so
    // this can only decommit pages we hold. The reservation itself survives, which is the whole
    // difference between MEM_DECOMMIT and MEM_RELEASE.
    let result = unsafe { VirtualFree(at.cast(), len, MEM_DECOMMIT) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
