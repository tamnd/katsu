//! A seeded generator, so that a failure is reproducible from one number in a log line.
//!
//! Hand written rather than pulled in, for two reasons. The first is the dependency budget in
//! spec 16, which is a budget rather than a preference. The second matters more: the value of a
//! fuzzer's seed is entirely in whether `--seed 41827` produces the same program tomorrow that it
//! produced today, and that is a promise about a specific algorithm rather than about a crate
//! version range. Pinning the algorithm here means a seed in a bug report stays meaningful across
//! every future version of everything else.
//!
//! The algorithm is SplitMix64, which is the one Java's `SplittableRandom` uses and the one the
//! `rand` crate seeds its own generators with. It is a counter through a fixed mixing function, so
//! it has no bad states, needs no warmup, and is about ten instructions. Statistical quality beyond
//! that is not something a program generator can spend, because the structure being generated is
//! several orders of magnitude coarser than any test a generator would fail.

/// A seeded source of numbers.
#[derive(Clone, Debug)]
pub(crate) struct Random {
    state: u64,
}

impl Random {
    /// Start from a seed, which is the whole reproducibility contract.
    pub(crate) const fn new(seed: u64) -> Random {
        Random { state: seed }
    }

    /// The next raw value.
    pub(crate) const fn next(&mut self) -> u64 {
        // SplitMix64, from Steele, Lea and Flood. The constants are the published ones and changing
        // any of them would silently invalidate every seed ever written down.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `bound`, or zero if the bound is zero.
    ///
    /// Lemire's multiply and shift rather than a modulo. The bias is one part in two to the sixty
    /// four for the bounds this is called with, which are all under a hundred, and the alternative
    /// is a rejection loop whose worst case is unbounded. For choosing a grammar production that is
    /// not a trade worth thinking about twice.
    pub(crate) const fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        // `as` rather than `u128::from`, which is not callable in a const function yet. The cast is
        // a widening one, so it is the same value either way.
        let value = self.next() as u128;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the shift leaves a value below bound, which is a usize by construction"
        )]
        {
            ((value * bound as u128) >> 64) as usize
        }
    }

    /// One item out of a slice, which is most of what a grammar needs.
    ///
    /// # Panics
    ///
    /// Panics if the slice is empty, which is a grammar with a production that has no alternatives
    /// and therefore a bug in the generator rather than in the program under test.
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        assert!(!items.is_empty(), "a production with no alternatives");
        &items[self.below(items.len())]
    }

    /// True with probability one in `n`.
    pub(crate) const fn chance(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::Random;

    #[test]
    fn the_same_seed_gives_the_same_sequence() {
        // The entire point of the file. A seed in a bug report that does not reproduce is worse
        // than no seed, because somebody spends an afternoon on it before concluding that.
        let first: Vec<u64> = (0..8).map(|_| Random::new(42).next()).collect();
        let mut generator = Random::new(42);
        let second: Vec<u64> = (0..8).map(|_| generator.next()).collect();
        assert_eq!(first[0], second[0]);

        let mut a = Random::new(7);
        let mut b = Random::new(7);
        let left: Vec<u64> = (0..64).map(|_| a.next()).collect();
        let right: Vec<u64> = (0..64).map(|_| b.next()).collect();
        assert_eq!(left, right);
    }

    #[test]
    fn different_seeds_give_different_sequences() {
        let mut a = Random::new(1);
        let mut b = Random::new(2);
        let left: Vec<u64> = (0..16).map(|_| a.next()).collect();
        let right: Vec<u64> = (0..16).map(|_| b.next()).collect();
        assert_ne!(left, right);
    }

    #[test]
    fn a_zero_seed_is_not_a_stuck_state() {
        // A plain xorshift is all zeroes forever from a zero seed, and zero is the default seed
        // anybody types first. A counter based generator has no such state, and this says so.
        let mut generator = Random::new(0);
        let values: Vec<u64> = (0..8).map(|_| generator.next()).collect();
        assert!(values.iter().any(|value| *value != 0));
        assert_ne!(values[0], values[1]);
    }

    #[test]
    fn a_bound_is_respected_and_a_zero_bound_does_not_divide_by_zero() {
        let mut generator = Random::new(99);
        for _ in 0..1000 {
            assert!(generator.below(7) < 7);
        }
        assert_eq!(generator.below(0), 0);
    }

    #[test]
    fn every_value_below_a_small_bound_comes_up() {
        // Not a statistical test, a wiring test. A multiply and shift that got the shift wrong
        // returns zero every time and every generated program would be the same program.
        let mut generator = Random::new(5);
        let mut seen = [false; 5];
        for _ in 0..500 {
            seen[generator.below(5)] = true;
        }
        assert!(seen.iter().all(|hit| *hit), "{seen:?}");
    }
}
