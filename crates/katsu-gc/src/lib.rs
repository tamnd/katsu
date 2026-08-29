//! The garbage collector interface and the binding to whichever collector we pick.
//!
//! The collector itself is a third party library. Which one is open question Q3 in
//! `spec/17-open-questions.md`, decided by measurement at M4 rather than by argument now.
//! This crate exists so that the decision is a swap behind a trait instead of a rewrite.
//! See `spec/08-gc-and-memory.md`.

mod atom;
mod bump;
mod cage;
mod function;
mod object;
mod ordinary;
mod shape;
mod string;

pub use atom::{Atom, AtomTable};
pub use bump::{BumpHeap, Census, KindTotals, ObjectKind};
pub use cage::{CAGE_SIZE, Cage, CageError, GUARD_SIZE, OBJECT_ALIGN, SMI_MAX, SMI_MIN, Slot};
pub use function::{AccessorPairRef, ClosureRef, ContextRef, NativeRef};
pub use object::HeapKind;
pub use ordinary::ObjectRef;
pub use shape::{Attributes, ShapeRef};
pub use string::{LoneSurrogate, MAX_STRING_LENGTH, STRING_HEADER_SIZE, StringRef, hash_str};

use std::fmt;

/// Why a collection was triggered.
///
/// Recorded on every cycle, because a heap that is collecting for the wrong reason is the
/// first symptom of a tuning problem and it is invisible without this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcReason {
    /// The nursery filled up. The common case, and it should stay the common case.
    AllocationFailed,
    /// The heap crossed the size the policy allows before a major collection.
    HeapLimit,
    /// An embedder or a test asked for it explicitly.
    Explicit,
    /// The process is idle and the collector took the opportunity.
    Idle,
}

/// What one collection cycle cost.
#[derive(Clone, Copy, Debug)]
pub struct GcStats {
    /// Bytes live after the cycle finished.
    pub live_bytes: usize,
    /// Bytes the collector has reserved from the operating system.
    pub reserved_bytes: usize,
    /// Total time the mutator was stopped, in microseconds.
    ///
    /// The sum of the pauses matters less than the distribution, so `spec/15-benchmarks.md`
    /// reports p99 and p99.9 rather than a mean.
    pub pause_micros: u64,
    /// Why the cycle happened.
    pub reason: GcReason,
}

/// The surface every collector binding has to provide.
///
/// Kept small on purpose. Anything that is not in this trait is something the rest of the
/// runtime is not allowed to assume about the collector, which is what keeps the M4
/// decision cheap.
pub trait Collector: fmt::Debug + Send {
    /// Allocate `bytes` of object storage, returning `None` if a collection is needed first.
    ///
    /// The fast path is expected to be inlined into generated code, so this is the slow
    /// path signature rather than the one tier 1 emits.
    fn try_allocate(&mut self, bytes: usize) -> Option<*mut u8>;

    /// Run a collection cycle and report what it cost.
    fn collect(&mut self, reason: GcReason) -> GcStats;

    /// Bytes currently reserved from the operating system.
    ///
    /// This is the number the 4 MiB idle budget in `spec/02-the-10x-goal.md` is measured
    /// against, and it is deliberately the reserved figure and not the live figure, because
    /// a container's memory limit counts pages and not liveness.
    fn reserved_bytes(&self) -> usize;

    /// A human readable name for the collector, printed by `katsu --heap-census`.
    fn name(&self) -> &'static str;
}
