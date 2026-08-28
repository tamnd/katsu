//! The checked in list of what currently passes, and the comparison that guards it.
//!
//! A raw pass rate is a bad regression test on a project that is going to spend a long time well
//! below a hundred percent, because nobody can tell a number that dropped by four from a number
//! that dropped by four for a reason. A set of names can be diffed, and a diff says exactly which
//! test stopped working.
//!
//! # Why the passing set rather than the failing set
//!
//! Both directions are a ratchet and they fail differently. Recording failures means the file
//! shrinks as the engine improves, and a test that quietly stops running at all, because a skip
//! rule grew or a file was renamed, looks exactly like a fix. Recording passes means a test that
//! stops running disappears from the passing set and is reported as a regression, which is the
//! correct answer, because a test that no longer runs is a test we no longer know the answer to.
//!
//! Both directions are also large eventually. The passing set is small today and grows toward fifty
//! thousand lines, which is a real cost in a repository and is worth paying for a file where every
//! line is a promise rather than an apology.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The file on disk.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct Expectations {
    /// A sentence for whoever opens the file wondering what it is.
    pub(crate) about: String,
    /// The suite revision this was taken against, when it could be determined.
    ///
    /// Recorded because the suite changes weekly and this file is a set of names out of it. Running
    /// against a different revision produces a diff full of tests that were added or renamed, which
    /// looks exactly like a regression until you know to check. The runner prints a warning on a
    /// mismatch rather than refusing to run, because comparing across revisions is a thing somebody
    /// deliberately wants to do sometimes.
    #[serde(default)]
    pub(crate) suite: String,
    /// The counts at the time it was written, for a human reading the diff.
    ///
    /// Not used by the comparison, which works entirely off the names. They are here because a
    /// pull request that moves the passing set should show what it did to the totals without
    /// anybody having to count lines in a diff.
    pub(crate) summary: Summary,
    /// Every case that passed, by the name the runner gives it.
    pub(crate) passing: BTreeSet<String>,
}

/// The counts from one run.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Summary {
    /// Cases built and considered, including the skipped ones.
    pub(crate) total: usize,
    /// Cases that did what their file said they would.
    pub(crate) passed: usize,
    /// Cases that did not.
    pub(crate) failed: usize,
    /// Cases that reached something this build has not implemented.
    pub(crate) unsupported: usize,
    /// Cases not run, each with a reason printed in the report.
    pub(crate) skipped: usize,
}

impl Summary {
    /// The pass rate against everything that was actually attempted.
    ///
    /// Skips are out of the denominator here and are printed next to it, because a rate that
    /// counts a test we refused to run as a test we failed is a rate nobody can act on, and a rate
    /// that hides how many we refused to run is one nobody should believe. Unsupported cases stay
    /// in, because those are the work.
    #[must_use]
    pub(crate) fn rate(self) -> f64 {
        let attempted = self.total.saturating_sub(self.skipped);
        if attempted == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "the suite is fifty thousand tests, not nine quadrillion"
        )]
        {
            self.passed as f64 / attempted as f64 * 100.0
        }
    }
}

/// What changed between the file and this run.
#[derive(Debug, Default)]
pub(crate) struct Difference {
    /// Cases the file says pass and that did not pass this time.
    pub(crate) regressed: Vec<String>,
    /// Cases that passed this time and are not in the file.
    pub(crate) improved: Vec<String>,
}

impl Difference {
    /// Whether the run matches the file exactly.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.regressed.is_empty() && self.improved.is_empty()
    }
}

impl Expectations {
    /// Build a file from a finished run.
    #[must_use]
    pub(crate) fn from_run(
        suite: String,
        summary: Summary,
        passing: BTreeSet<String>,
    ) -> Expectations {
        Expectations {
            about: "Every test262 case that passes, written by tools/test262-runner with --bless. \
                    A case listed here that stops passing is a regression. A case that starts \
                    passing and is not here fails the check too, so that an improvement has to be \
                    committed rather than quietly banked."
                .to_owned(),
            suite,
            summary,
            passing,
        }
    }

    /// Read the file, or an empty one if it has never been written.
    ///
    /// Missing is not an error, because the first run on a fresh checkout has nothing to compare
    /// against and should still report a number.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists and is not readable or not valid JSON, which means
    /// somebody edited it by hand and the answer is to regenerate rather than to guess.
    pub(crate) fn read(path: &Path) -> Result<Expectations> {
        if !path.exists() {
            return Ok(Expectations::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    /// Write the file.
    ///
    /// Pretty printed with a trailing newline, because it is a source file that lives in a diff.
    /// Compact JSON would put fifty thousand names on one line and every change to it would be
    /// unreviewable.
    ///
    /// # Errors
    ///
    /// Returns an error if the file or its directory cannot be written.
    pub(crate) fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
    }

    /// Compare a run against this file.
    #[must_use]
    pub(crate) fn compare(&self, passing: &BTreeSet<String>) -> Difference {
        Difference {
            regressed: self.passing.difference(passing).cloned().collect(),
            improved: passing.difference(&self.passing).cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Expectations, Summary};

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_run_that_matches_the_file_has_nothing_to_say() {
        let file =
            Expectations::from_run(String::new(), Summary::default(), set(&["a.js", "b.js"]));
        assert!(file.compare(&set(&["a.js", "b.js"])).is_empty());
    }

    #[test]
    fn a_test_that_stops_passing_is_named() {
        let file =
            Expectations::from_run(String::new(), Summary::default(), set(&["a.js", "b.js"]));
        let difference = file.compare(&set(&["a.js"]));
        assert_eq!(difference.regressed, ["b.js"]);
        assert!(difference.improved.is_empty());
    }

    #[test]
    fn a_test_that_starts_passing_also_fails_the_check() {
        // Deliberately not silent. An improvement that is not committed is an improvement the next
        // person's regression check does not know about, which is how a ratchet loosens.
        let file = Expectations::from_run(String::new(), Summary::default(), set(&["a.js"]));
        let difference = file.compare(&set(&["a.js", "b.js"]));
        assert_eq!(difference.improved, ["b.js"]);
        assert!(!difference.is_empty());
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_one_rather_than_an_error() {
        // The first run on a fresh checkout should still report a number.
        let file = Expectations::read(std::path::Path::new("/nonexistent/expectations.json"))
            .expect("missing is fine");
        assert!(file.passing.is_empty());
    }

    #[test]
    fn the_rate_is_out_of_what_was_attempted_and_not_out_of_everything() {
        let summary = Summary {
            total: 100,
            passed: 10,
            failed: 20,
            unsupported: 40,
            skipped: 30,
        };
        // Ten of the seventy we tried, not ten of a hundred. Counting a test we refused to run as
        // one we failed produces a number nobody can act on.
        assert!(
            (summary.rate() - 14.285_714).abs() < 0.001,
            "{}",
            summary.rate()
        );
    }

    #[test]
    fn a_run_with_nothing_attempted_is_zero_and_not_a_division_by_zero() {
        let summary = Summary {
            total: 5,
            skipped: 5,
            ..Summary::default()
        };
        assert!((summary.rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_file_round_trips_through_json() {
        let temporary = tempfile::tempdir().expect("temp dir");
        let path = temporary.path().join("nested").join("expectations.json");
        let written = Expectations::from_run(
            "abc123".to_owned(),
            Summary {
                total: 3,
                passed: 1,
                ..Summary::default()
            },
            set(&["a.js"]),
        );
        written.write(&path).expect("should write");
        let read = Expectations::read(&path).expect("should read");
        assert_eq!(read.passing, written.passing);
        assert_eq!(read.summary, written.summary);
        assert_eq!(read.suite, "abc123");
    }
}
