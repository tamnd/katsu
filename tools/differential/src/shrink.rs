//! Cutting a divergence down to something a person can read.
//!
//! A generated program is forty lines of parenthesised nonsense and the disagreement is in one of
//! them. A report that prints all forty gets skimmed and filed, and a report that prints three
//! gets fixed the same afternoon, so the shrinker is not a nicety. It is the difference between
//! finding bugs and producing bug shaped noise.
//!
//! The strategy is the simplest one that works: try the program with each statement removed, keep
//! any removal that still disagrees, and go around again until a whole pass changes nothing. It is
//! quadratic in the number of statements and the programs are tens of statements, so the cost is
//! tens of runs of a program that takes a millisecond, paid only when something already went wrong.
//!
//! # Why removals that break the program are safe
//!
//! Taking out a `let` leaves the lines that used it referring to a name that no longer exists, and
//! both engines report the same `ReferenceError` for that. The predicate sees agreement and the
//! removal is rejected, so a declaration something depends on simply stays. Nothing needs to
//! understand the program's data flow, which is what keeps this thirty lines instead of three
//! hundred.

use crate::generate::Program;

/// Remove as much as possible while the program still disagrees.
///
/// `diverges` is asked about candidate programs and must be the same question that produced the
/// original report. A predicate that is not deterministic will shrink to something arbitrary, which
/// is worth knowing about because it means the divergence itself was not deterministic.
pub(crate) fn shrink(program: &Program, mut diverges: impl FnMut(&Program) -> bool) -> Program {
    let mut best = program.clone();
    loop {
        let mut smaller = None;
        for index in (0..best.statements.len()).rev() {
            let candidate = best.without(index);
            if candidate.statements.is_empty() {
                continue;
            }
            if diverges(&candidate) {
                smaller = Some(candidate);
                break;
            }
        }
        match smaller {
            Some(candidate) => best = candidate,
            // A whole pass with nothing removable, which is the definition of done. Stopping on the
            // first pass that changes nothing rather than after a fixed number of rounds, because
            // the fixed number is always either too few on the bad day or wasted on every other.
            None => return best,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shrink;
    use crate::generate::Program;

    fn program(statements: &[&str]) -> Program {
        Program {
            seed: 1,
            statements: statements.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    #[test]
    fn everything_that_is_not_the_cause_goes_away() {
        let full = program(&["a();", "b();", "console.log(0.1 + 0.2);", "c();", "d();"]);
        let smallest = shrink(&full, |candidate| candidate.source().contains("0.1 + 0.2"));
        assert_eq!(smallest.statements, vec!["console.log(0.1 + 0.2);"]);
    }

    #[test]
    fn two_statements_that_are_both_needed_both_stay() {
        // The case a shrinker that removes one at a time and stops would get wrong. The divergence
        // needs the declaration and the use, and neither alone reproduces it.
        let full = program(&["x();", "let v = 1;", "y();", "console.log(v);", "z();"]);
        let smallest = shrink(&full, |candidate| {
            let source = candidate.source();
            source.contains("let v = 1;") && source.contains("console.log(v);")
        });
        assert_eq!(smallest.statements, vec!["let v = 1;", "console.log(v);"]);
    }

    #[test]
    fn a_program_that_cannot_shrink_comes_back_unchanged() {
        let full = program(&["one();", "two();"]);
        let smallest = shrink(&full, |candidate| candidate.statements.len() == 2);
        assert_eq!(smallest.statements, full.statements);
    }

    #[test]
    fn shrinking_never_returns_an_empty_program() {
        // A predicate that says yes to everything would otherwise shrink to nothing at all, and a
        // report of an empty program is a report nobody can act on.
        let full = program(&["one();", "two();", "three();"]);
        let smallest = shrink(&full, |_| true);
        assert_eq!(smallest.statements.len(), 1);
    }

    #[test]
    fn the_seed_survives_shrinking() {
        // The shrunk program is what gets printed and the seed is what gets rerun, so losing the
        // seed here would make every report unreproducible in exactly the situation it matters.
        let full = program(&["one();", "two();"]);
        let smallest = shrink(&full, |candidate| !candidate.statements.is_empty());
        assert_eq!(smallest.seed, full.seed);
    }
}
