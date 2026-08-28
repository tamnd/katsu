//! Bump allocation into the cage, with no collector behind it.
//!
//! This is the M0 heap. It hands out memory and never takes any back, which is fine for a
//! milestone whose exit criterion is printing from a trivial script and is obviously not fine
//! for anything else. The real collector arrives at M4, and which collector it is is open
//! question Q3 in `spec/17-open-questions.md`, decided by measurement rather than by argument.
//!
//! Two things here outlive this milestone. The first is bump allocation itself: whatever Q3
//! decides, the allocation fast path is a pointer increment and a limit check, because that is
//! what every generational and every region based collector gives you. The second is the census,
//! which is the point of building this now rather than using a `Vec<u8>`. The memory budget in
//! `spec/02-the-10x-goal.md` 2.3 is a table of line items, `spec/08-gc-and-memory.md` 8.7 says a
//! budget that is not a test is a wish, and a test needs something to read from. That something
//! starts here.

use std::fmt;
use std::ptr::NonNull;

use crate::cage::{CAGE_SIZE, Cage, CageError, OBJECT_ALIGN};
use crate::{Collector, GcReason, GcStats};

/// How much the heap commits at a time as it grows.
///
/// Sixty four kilobytes. Small enough that a process which allocates almost nothing stays near
/// the bottom of the 512 KiB initial heap line in the 2.3 budget, large enough that the commit
/// syscall is not on any path that runs often.
const COMMIT_CHUNK: usize = 64 * 1024;

/// What an allocation was for.
///
/// The categories are the line items the heap census reports, so they are chosen to match the
/// questions somebody asks when the memory number regresses, rather than to match the type
/// hierarchy. A number that says "objects grew by 3 MB" is actionable and a number that says
/// "the heap grew by 3 MB" is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObjectKind {
    /// An ordinary object, including its inline slots.
    Object,
    /// An array's backing store.
    Elements,
    /// A string in any of its representations.
    String,
    /// A closure, not counting the shared blueprint.
    Closure,
    /// A function written in Rust, not counting the table entry that holds the code.
    ///
    /// Its own line rather than a closure, because these are allocated once each when a realm is
    /// built and never again, so a number that moves here means a builtin was added and a number
    /// that moves in `closures` means a program made a function. Two different questions.
    Native,
    /// One level of environment, holding the variables a nested function captured.
    Context,
    /// A shape, which is shared across every object with that layout.
    Shape,
    /// A property array that overflowed the inline slots.
    Properties,
    /// Anything the runtime has not categorised yet.
    ///
    /// This exists so that the census adds up. A category that quietly drops allocations is
    /// worse than no category, because it looks like a measurement.
    Other,
}

impl ObjectKind {
    /// Every category, in the order the census prints them.
    pub const ALL: [ObjectKind; 9] = [
        ObjectKind::Object,
        ObjectKind::Elements,
        ObjectKind::String,
        ObjectKind::Closure,
        ObjectKind::Native,
        ObjectKind::Context,
        ObjectKind::Shape,
        ObjectKind::Properties,
        ObjectKind::Other,
    ];

    /// The name printed by `katsu --heap-census`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ObjectKind::Object => "objects",
            ObjectKind::Elements => "elements",
            ObjectKind::String => "strings",
            ObjectKind::Closure => "closures",
            ObjectKind::Native => "natives",
            ObjectKind::Context => "contexts",
            ObjectKind::Shape => "shapes",
            ObjectKind::Properties => "properties",
            ObjectKind::Other => "other",
        }
    }

    const fn index(self) -> usize {
        match self {
            ObjectKind::Object => 0,
            ObjectKind::Elements => 1,
            ObjectKind::String => 2,
            ObjectKind::Closure => 3,
            ObjectKind::Native => 4,
            ObjectKind::Context => 5,
            ObjectKind::Shape => 6,
            ObjectKind::Properties => 7,
            ObjectKind::Other => 8,
        }
    }
}

/// One category's running totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindTotals {
    /// How many allocations of this kind have happened.
    pub count: u64,
    /// How many bytes they asked for, before alignment padding.
    pub requested_bytes: u64,
    /// How many bytes they actually took, after alignment padding.
    ///
    /// The gap between this and `requested_bytes` is the alignment tax, and having it as a
    /// separate number is how an object layout that wastes four bytes per instance gets noticed
    /// rather than being absorbed into a total.
    pub reserved_bytes: u64,
}

/// What the heap has done since it was created.
///
/// Every number here is cumulative and monotonic. There is no collector yet, so nothing can go
/// down, and when the collector arrives the live figures will be separate fields rather than
/// these ones learning to decrease.
#[derive(Clone, Copy, Debug, Default)]
pub struct Census {
    totals: [KindTotals; 9],
    /// Bytes handed out in total, after alignment padding.
    pub allocated_bytes: u64,
    /// Allocations in total.
    pub allocation_count: u64,
    /// Bytes committed from the operating system.
    ///
    /// This is the number the 4 MiB idle budget in spec 2.3 is measured against. It is not the
    /// size of the reservation, which is eight gigabytes of address space and costs nothing, and
    /// it is not the number of live bytes, because a container's memory limit counts pages and
    /// has no opinion about liveness.
    pub committed_bytes: usize,
    /// Times the heap asked the operating system for more pages.
    pub commit_count: u64,
    /// Bytes lost to alignment padding.
    pub padding_bytes: u64,
}

impl Census {
    /// The totals for one category.
    #[must_use]
    pub const fn totals(&self, kind: ObjectKind) -> KindTotals {
        self.totals[kind.index()]
    }

    /// A census report, one line per category, in the shape `katsu --heap-census` prints.
    ///
    /// Categories with nothing in them are printed as zeroes rather than omitted, because a
    /// missing line reads as "this cannot happen" when it means "this did not happen this time".
    #[must_use]
    pub fn report(&self) -> String {
        use fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{:<12} {:>10} {:>14} {:>14}",
            "kind", "count", "requested", "reserved"
        );
        for kind in ObjectKind::ALL {
            let totals = self.totals(kind);
            let _ = writeln!(
                out,
                "{:<12} {:>10} {:>14} {:>14}",
                kind.name(),
                totals.count,
                totals.requested_bytes,
                totals.reserved_bytes
            );
        }
        let _ = writeln!(
            out,
            "{:<12} {:>10} {:>14} {:>14}",
            "total", self.allocation_count, "", self.allocated_bytes
        );
        let _ = writeln!(
            out,
            "committed {} bytes in {} commits, {} bytes lost to alignment",
            self.committed_bytes, self.commit_count, self.padding_bytes
        );
        out
    }
}

/// A heap that only ever moves forward.
///
/// Allocation is a rounding, a comparison and an addition. Everything else here is bookkeeping
/// for the census, and the bookkeeping deliberately lives on this side of the fast path rather
/// than being sampled, because a sampled allocation count is not something a budget test can
/// fail on.
pub struct BumpHeap {
    cage: Cage,
    /// Offset of the next free byte.
    cursor: usize,
    /// Offset one past the last committed byte.
    committed: usize,
    census: Census,
}

// The cage prints its own base and size and the census is a dozen counters, neither of which
// belongs in the one line somebody wants when they are asking where the heap got to.
#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for BumpHeap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BumpHeap")
            .field("cursor", &self.cursor)
            .field("committed", &self.committed)
            .field("allocations", &self.census.allocation_count)
            .finish()
    }
}

impl BumpHeap {
    /// Reserve a cage and start a heap at the bottom of it.
    ///
    /// Nothing is committed here. A heap that has not been allocated into costs address space
    /// and no pages, which is what the "before any user allocation" line in the 2.3 budget is
    /// asking for.
    ///
    /// # Errors
    ///
    /// Returns [`CageError`] if the operating system refuses the reservation.
    pub fn new() -> Result<BumpHeap, CageError> {
        Ok(BumpHeap {
            cage: Cage::new()?,
            cursor: 0,
            committed: 0,
            census: Census::default(),
        })
    }

    /// The cage this heap allocates into.
    #[must_use]
    pub const fn cage(&self) -> &Cage {
        &self.cage
    }

    /// What the heap has done so far.
    #[must_use]
    pub const fn census(&self) -> &Census {
        &self.census
    }

    /// Allocate `bytes` for an object of `kind`, returning a pointer and its cage offset.
    ///
    /// The result is eight byte aligned and the memory is zero, because it comes from pages the
    /// kernel just handed over and this heap never reuses anything.
    ///
    /// Returns `None` when the cage is full, which at M0 means the process is out of heap with
    /// no recourse. That is the honest behaviour for a heap with no collector, and it is a
    /// better one than growing past the cage and losing pointer compression.
    pub fn allocate(&mut self, bytes: usize, kind: ObjectKind) -> Option<NonNull<u8>> {
        if bytes == 0 {
            return None;
        }
        let reserved = bytes.checked_next_multiple_of(OBJECT_ALIGN)?;
        let end = self.cursor.checked_add(reserved)?;
        if end > CAGE_SIZE {
            return None;
        }

        if end > self.committed {
            let target = end.next_multiple_of(COMMIT_CHUNK).min(CAGE_SIZE);
            self.cage.commit_range(self.committed, target).ok()?;
            self.census.commit_count += 1;
            self.committed = target;
            self.census.committed_bytes = target;
        }

        let offset = self.cursor;
        self.cursor = end;

        let totals = &mut self.census.totals[kind.index()];
        totals.count += 1;
        totals.requested_bytes += bytes as u64;
        totals.reserved_bytes += reserved as u64;
        self.census.allocation_count += 1;
        self.census.allocated_bytes += reserved as u64;
        self.census.padding_bytes += (reserved - bytes) as u64;

        // The offset always fits, because the cursor is bounded by the cage size above.
        NonNull::new(self.cage.address_of(u32::try_from(offset).ok()?))
    }

    /// Commit enough pages that the next `bytes` of allocation will not need a syscall.
    ///
    /// The cursor does not move, so this reserves capacity rather than space. A caller who knows
    /// roughly how much a phase will allocate can pay for the pages once instead of once per
    /// chunk, which matters at startup where the syscalls are a measurable share of a cold start
    /// that `spec/02-the-10x-goal.md` wants ten times faster than Node's.
    ///
    /// Committing ahead costs resident memory immediately, so this is deliberately something a
    /// caller asks for rather than something the heap does on a guess.
    ///
    /// # Errors
    ///
    /// Returns [`CageError`] if the range leaves the cage or the kernel refuses.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), CageError> {
        let target = self
            .cursor
            .saturating_add(bytes)
            .next_multiple_of(COMMIT_CHUNK)
            .min(CAGE_SIZE);
        if target <= self.committed {
            return Ok(());
        }
        self.cage.commit_range(self.committed, target)?;
        self.census.commit_count += 1;
        self.committed = target;
        self.census.committed_bytes = target;
        Ok(())
    }

    /// The offset of the next byte that will be allocated.
    ///
    /// Exposed for tests and for the census, not as something the runtime should reason about.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }
}

impl Collector for BumpHeap {
    fn try_allocate(&mut self, bytes: usize) -> Option<*mut u8> {
        self.allocate(bytes, ObjectKind::Other).map(NonNull::as_ptr)
    }

    fn collect(&mut self, reason: GcReason) -> GcStats {
        // There is no collector, and saying so by reporting every byte as live is the only
        // honest answer. Returning a smaller live figure here would make the M1 census look
        // better than the heap actually is, which is exactly the kind of number this project
        // exists to not publish.
        GcStats {
            live_bytes: usize::try_from(self.census.allocated_bytes).unwrap_or(usize::MAX),
            reserved_bytes: self.committed,
            pause_micros: 0,
            reason,
        }
    }

    fn reserved_bytes(&self) -> usize {
        // Committed, not reserved from the address space. The cage reservation is eight
        // gigabytes and costs nothing, and reporting it here would make every memory number in
        // the project meaningless.
        self.committed
    }

    fn name(&self) -> &'static str {
        "bump (no collector, M0)"
    }
}

#[cfg(test)]
mod tests {
    use super::{BumpHeap, COMMIT_CHUNK, Census, ObjectKind};
    use crate::cage::OBJECT_ALIGN;
    use crate::{Collector, GcReason};

    #[test]
    fn a_fresh_heap_has_committed_nothing() {
        let heap = BumpHeap::new().unwrap();
        assert_eq!(heap.census().committed_bytes, 0);
        assert_eq!(heap.reserved_bytes(), 0);
        assert_eq!(heap.cursor(), 0);
    }

    #[test]
    fn allocations_are_aligned_distinct_and_zeroed() {
        let mut heap = BumpHeap::new().unwrap();
        let mut seen = Vec::new();
        for size in 1..=200usize {
            let pointer = heap.allocate(size, ObjectKind::Object).unwrap();
            assert_eq!(
                pointer.as_ptr() as usize % OBJECT_ALIGN,
                0,
                "allocation of {size} bytes was not aligned"
            );
            assert!(heap.cage().contains(pointer.as_ptr()));

            // SAFETY: the heap just handed out this range and never hands out an overlapping
            // one, so this test has exclusive access to it for the duration of the block.
            unsafe {
                for byte in 0..size {
                    assert_eq!(
                        pointer.as_ptr().add(byte).read(),
                        0,
                        "fresh memory must be zero"
                    );
                    pointer.as_ptr().add(byte).write(0xCD);
                }
            }
            seen.push(pointer.as_ptr() as usize);
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            seen.len(),
            "two allocations shared an address"
        );
    }

    #[test]
    fn the_census_adds_up() {
        let mut heap = BumpHeap::new().unwrap();
        heap.allocate(24, ObjectKind::Object).unwrap();
        heap.allocate(24, ObjectKind::Object).unwrap();
        heap.allocate(13, ObjectKind::String).unwrap();
        heap.allocate(64, ObjectKind::Shape).unwrap();

        let census = heap.census();
        assert_eq!(census.allocation_count, 4);
        assert_eq!(census.totals(ObjectKind::Object).count, 2);
        assert_eq!(census.totals(ObjectKind::Object).requested_bytes, 48);
        assert_eq!(census.totals(ObjectKind::Object).reserved_bytes, 48);

        // Thirteen bytes rounds up to sixteen, so three bytes of padding, and that has to show
        // up as padding rather than disappearing into the total.
        assert_eq!(census.totals(ObjectKind::String).requested_bytes, 13);
        assert_eq!(census.totals(ObjectKind::String).reserved_bytes, 16);
        assert_eq!(census.padding_bytes, 3);

        let summed: u64 = ObjectKind::ALL
            .iter()
            .map(|&kind| census.totals(kind).reserved_bytes)
            .sum();
        assert_eq!(
            summed, census.allocated_bytes,
            "the per kind totals have to sum to the overall total or the census is decorative"
        );
        assert_eq!(
            usize::try_from(census.allocated_bytes).unwrap(),
            heap.cursor()
        );
    }

    #[test]
    fn the_heap_commits_in_chunks_rather_than_per_allocation() {
        let mut heap = BumpHeap::new().unwrap();
        for _ in 0..1000 {
            heap.allocate(8, ObjectKind::Object).unwrap();
        }
        assert_eq!(heap.cursor(), 8000);
        assert_eq!(heap.census().committed_bytes, COMMIT_CHUNK);
        assert_eq!(
            heap.census().commit_count,
            1,
            "eight thousand bytes should fit in one commit"
        );

        // Cross the chunk boundary and check that exactly one more commit happens.
        for _ in 0..(COMMIT_CHUNK / 8) {
            heap.allocate(8, ObjectKind::Object).unwrap();
        }
        assert_eq!(heap.census().commit_count, 2);
        assert_eq!(heap.census().committed_bytes, COMMIT_CHUNK * 2);
    }

    #[test]
    fn reserving_ahead_removes_the_commits_from_the_allocation_path() {
        let mut heap = BumpHeap::new().unwrap();
        heap.reserve(1 << 20).unwrap();
        assert_eq!(heap.census().commit_count, 1);
        assert!(heap.census().committed_bytes >= 1 << 20);
        assert_eq!(
            heap.cursor(),
            0,
            "reserving capacity must not consume space"
        );

        for _ in 0..1000 {
            heap.allocate(64, ObjectKind::Object).unwrap();
        }
        assert_eq!(
            heap.census().commit_count,
            1,
            "sixty four kilobytes of allocation inside a one megabyte reservation should not \
             have touched the kernel again"
        );

        // Asking for less than is already committed is free rather than a second syscall.
        heap.reserve(1024).unwrap();
        assert_eq!(heap.census().commit_count, 1);
    }

    #[test]
    fn a_zero_byte_allocation_is_refused() {
        // Handing back a pointer to nothing means two objects share an address, which breaks
        // identity. Every real object has a header, so nothing legitimate asks for zero.
        let mut heap = BumpHeap::new().unwrap();
        assert!(heap.allocate(0, ObjectKind::Object).is_none());
    }

    #[test]
    fn an_allocation_larger_than_the_cage_is_refused_rather_than_wrapping() {
        let mut heap = BumpHeap::new().unwrap();
        assert!(heap.allocate(usize::MAX, ObjectKind::Elements).is_none());
        assert!(
            heap.allocate(super::CAGE_SIZE + 1, ObjectKind::Elements)
                .is_none()
        );
        assert_eq!(
            heap.cursor(),
            0,
            "a refused allocation must not move the cursor"
        );
    }

    #[test]
    fn collecting_reports_everything_as_live_because_nothing_is_collected() {
        let mut heap = BumpHeap::new().unwrap();
        heap.allocate(128, ObjectKind::Object).unwrap();
        let stats = heap.collect(GcReason::Explicit);
        assert_eq!(stats.live_bytes, 128);
        assert_eq!(stats.reason, GcReason::Explicit);
        assert_eq!(stats.pause_micros, 0);
        assert!(heap.name().contains("no collector"));
    }

    #[test]
    fn the_report_names_every_category_even_the_empty_ones() {
        let census = Census::default();
        let report = census.report();
        for kind in ObjectKind::ALL {
            assert!(
                report.contains(kind.name()),
                "the report dropped {}, which reads as impossible rather than as zero",
                kind.name()
            );
        }
    }
}
