//! Operating system specifics: executable memory, W^X, mmap and signals.
//!
//! Everything in the runtime that has to know what platform it is on lives here, so that
//! the layers above it can stay portable. See `spec/03-architecture.md` for the layer stack
//! and `spec/06-jit-tiers.md` for the write xor execute rules this crate has to enforce.

mod reservation;
mod sys;

pub use reservation::{Reservation, ReservationError};

/// The page size the running kernel is using.
///
/// The memory budget in `spec/02-the-10x-goal.md` is written in pages rather than bytes,
/// because a 4 KiB budget on a 16 KiB page machine is not a budget, it is a rounding error.
#[must_use]
pub fn page_size() -> usize {
    sys::page_size()
}

/// Round `bytes` up to a whole number of pages.
#[must_use]
pub fn round_up_to_page(bytes: usize) -> usize {
    let page = page_size();
    bytes.div_ceil(page) * page
}

/// Whether this platform requires per thread toggling of write and execute permission
/// rather than allowing a page to be mapped both ways.
///
/// Apple silicon under the hardened runtime does, which is why `spec/06-jit-tiers.md`
/// specifies `pthread_jit_write_protect_np` rather than a plain `mprotect` dance.
#[must_use]
pub const fn needs_jit_write_protect_toggle() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

#[cfg(test)]
mod tests {
    use super::{page_size, round_up_to_page};

    #[test]
    fn page_size_is_a_sane_power_of_two() {
        let page = page_size();
        assert!(page >= 4096, "page size {page} is implausibly small");
        assert!(
            page.is_power_of_two(),
            "page size {page} is not a power of two"
        );
    }

    #[test]
    fn rounding_up_lands_on_a_page_boundary() {
        let page = page_size();
        assert_eq!(round_up_to_page(0), 0);
        assert_eq!(round_up_to_page(1), page);
        assert_eq!(round_up_to_page(page), page);
        assert_eq!(round_up_to_page(page + 1), page * 2);
    }
}
