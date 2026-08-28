//! Tier 1, the copy and patch baseline JIT, and tier 2, the optimizing JIT.
//!
//! Tier 1 is generated from the same opcode description the interpreter is generated from,
//! so the two cannot drift apart. Tier 2 uses a control flow graph SSA intermediate
//! representation rather than sea of nodes, because V8 spent three years moving off sea of
//! nodes and reported compile time roughly halved. See `spec/06-jit-tiers.md`.

/// Which tier a function is currently executing in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// The interpreter. Most code in a real process never leaves this tier, which is why
    /// `spec/05-interpreter.md` treats it as a real tier and not a placeholder.
    Interpreter,
    /// The copy and patch baseline. Compilation costs microseconds.
    Baseline,
    /// The optimizing tier, entered only for code that is provably hot.
    Optimizing,
}

/// When a function moves up a tier.
///
/// Deliberately low. A baseline compile is cheap enough that waiting is the more expensive
/// mistake, and `spec/06-jit-tiers.md` sets the tier up threshold at eight invocations.
pub const BASELINE_TIER_UP_INVOCATIONS: u32 = 8;

/// Why the optimizing tier gave up and fell back to the interpreter.
///
/// Every reason is recorded, because a deoptimization loop is invisible in a profile and
/// obvious in a counter. See `spec/06-jit-tiers.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeoptReason {
    /// A shape guard failed. The overwhelmingly common case.
    ShapeMismatch,
    /// A value was not the type the speculation assumed.
    TypeMismatch,
    /// An integer operation overflowed out of the small integer range.
    Overflow,
    /// A path the optimizer had proven unreachable was reached after all.
    UnreachableReached,
}

/// Whether this build can compile at all.
///
/// False for an interpreter only build, which is a supported product and not a degraded
/// one: it is what runs on platforms that refuse writable executable memory, and it is the
/// smaller attack surface option from `spec/14-quality-bar.md`.
#[must_use]
pub fn jit_available() -> bool {
    katsu_stencils::stencils_available()
}

#[cfg(test)]
mod tests {
    use super::Tier;

    #[test]
    fn tiers_order_from_slowest_to_fastest() {
        assert!(Tier::Interpreter < Tier::Baseline);
        assert!(Tier::Baseline < Tier::Optimizing);
    }
}
