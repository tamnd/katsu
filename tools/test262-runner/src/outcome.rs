//! What happened to one test, and the rules for deciding it.
//!
//! Kept apart from the code that runs anything, because the rules are the part worth arguing about
//! and a pure function is the only version of them anybody can check.
//!
//! # The distinction the whole number rests on
//!
//! A conformance suite is roughly ten percent negative tests, files that are supposed to be
//! rejected. A runner that cannot tell "this is invalid JavaScript" from "this is valid JavaScript
//! the engine has not built yet" scores every one of those as a pass, because we do reject them,
//! just for entirely the wrong reason. That inflates the pass rate by exactly the amount of work
//! left to do, which is the most flattering and least useful direction for a number to be wrong in.
//!
//! So the runtime tells us which it is, [`katsu_runtime::Error::NotImplemented`] against
//! [`katsu_runtime::Error::Syntax`], and this file refuses to credit the first one. A negative test
//! that we reject for our own reasons is [`Outcome::Unsupported`] and is not counted as a pass.

use katsu_runtime::Error;

use crate::metadata::{Negative, Phase};

/// What one run of one file came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It did what the file said it would.
    Passed,
    /// It did not, and this is the difference.
    Failed(String),
    /// It reached something this build has not implemented, which is not the same as failing.
    ///
    /// Separated because these two numbers answer different questions. Failures are bugs and this
    /// is a work list, and adding them together produces a number that means neither thing.
    Unsupported(String),
    /// Not run at all, and why.
    Skipped(&'static str),
}

impl Outcome {
    /// The one word a table column wants.
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed(_) => "failed",
            Outcome::Unsupported(_) => "unsupported",
            Outcome::Skipped(_) => "skipped",
        }
    }

    /// The reason, for the histogram that turns this suite into a work list.
    pub(crate) fn reason(&self) -> &str {
        match self {
            Outcome::Passed => "",
            Outcome::Failed(text) | Outcome::Unsupported(text) => text,
            Outcome::Skipped(text) => text,
        }
    }
}

/// Decide what one completed run means, given what the file said should happen.
///
/// `result` is what evaluating the source produced. `negative` is the file's own expectation, or
/// `None` for the ordinary case of a file that is supposed to run cleanly.
pub(crate) fn judge(result: &Result<(), Error>, negative: Option<&Negative>) -> Outcome {
    match negative {
        None => positive(result),
        Some(expected) => negative_case(result, expected),
    }
}

/// A file that is supposed to run without complaining.
fn positive(result: &Result<(), Error>) -> Outcome {
    match result {
        Ok(()) => Outcome::Passed,
        // The gap is ours, so it is not a failure of the engine's behaviour, it is a piece of
        // engine that is missing. The message carries the construct, which is what makes the
        // histogram at the end of a run readable as a list of things to build.
        Err(Error::NotImplemented(text)) => Outcome::Unsupported(construct(text)),
        Err(Error::Syntax(text)) => Outcome::Failed(format!("rejected valid source: {text}")),
        Err(Error::Uncaught(text)) => Outcome::Failed(text.clone()),
        Err(Error::Fatal(text)) => Outcome::Failed(format!("fatal: {text}")),
    }
}

/// A file that is supposed to be rejected, in a named phase, with a named error.
fn negative_case(result: &Result<(), Error>, expected: &Negative) -> Outcome {
    match result {
        Ok(()) => Outcome::Failed(format!(
            "expected {} in the {} phase, ran cleanly instead",
            expected.kind,
            phase_name(expected.phase)
        )),

        // The important arm. We rejected it, but for our own reasons, so we have learned nothing
        // about whether we would reject it for the right reason once the gap is closed. Crediting
        // this is how a pass rate becomes a measure of how much is unimplemented.
        Err(Error::NotImplemented(text)) => Outcome::Unsupported(construct(text)),

        Err(Error::Syntax(_)) => {
            if expected.phase == Phase::Parse {
                // The phase is checked and the error name is not, because every error found before
                // anything runs is reported as a syntax error today, the parser having one error
                // type. That is only sound because every one of the 4657 parse phase negative tests
                // in the suite expects a SyntaxError, which was checked rather than assumed, so
                // there is nothing here for the missing comparison to get wrong. It stops being
                // sound if the suite ever adds one that expects something else, and the fix then is
                // an error kind on ParseError rather than a special case here.
                Outcome::Passed
            } else {
                Outcome::Failed(format!(
                    "expected {} in the {} phase, was rejected before running",
                    expected.kind,
                    phase_name(expected.phase)
                ))
            }
        }

        Err(Error::Uncaught(text)) => {
            if expected.phase == Phase::Parse {
                return Outcome::Failed(format!(
                    "expected {} before running, threw at runtime instead: {text}",
                    expected.kind
                ));
            }
            // The message starts with the constructor name, `TypeError: ...`, which is what the
            // file names. Comparing the name rather than the message, because the message is not
            // specified and every engine words it differently.
            if thrown_kind(text) == expected.kind {
                Outcome::Passed
            } else {
                Outcome::Failed(format!("expected {}, got {text}", expected.kind))
            }
        }

        Err(Error::Fatal(text)) => Outcome::Failed(format!("fatal: {text}")),
    }
}

/// The constructor name at the front of a thrown error's message.
///
/// `TypeError: x is not a function` gives `TypeError`. Anything without that shape gives the whole
/// text, which will not match and will be reported as a mismatch, which is the right outcome for a
/// message we cannot read.
fn thrown_kind(text: &str) -> &str {
    text.split_once(':').map_or(text, |(kind, _)| kind.trim())
}

/// The construct out of a not implemented message, which is the part worth counting.
///
/// The message is `not implemented yet: path:1:1: a for loop is not supported yet. See ...`, and
/// what a work list wants is `a for loop`. Keeping the path would make every one of fifty thousand
/// tests its own unique reason and the histogram would have fifty thousand rows of one.
fn construct(text: &str) -> String {
    let tail = text.rsplit_once(": ").map_or(text, |(_, tail)| tail);
    tail.trim_end_matches(" See https://github.com/tamnd/katsu/milestones")
        .trim_end()
        .trim_end_matches('.')
        .to_owned()
}

const fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Parse => "parse",
        Phase::Resolution => "resolution",
        Phase::Runtime => "runtime",
    }
}

#[cfg(test)]
mod tests {
    use katsu_runtime::Error;

    use super::{Outcome, construct, judge, thrown_kind};
    use crate::metadata::{Negative, Phase};

    fn negative(phase: Phase, kind: &str) -> Negative {
        Negative {
            phase,
            kind: kind.to_owned(),
        }
    }

    #[test]
    fn a_positive_test_that_runs_cleanly_passes() {
        assert_eq!(judge(&Ok(()), None), Outcome::Passed);
    }

    #[test]
    fn a_positive_test_that_throws_fails_with_the_message() {
        let error = Err(Error::Uncaught("TypeError: nope".to_owned()));
        assert_eq!(
            judge(&error, None),
            Outcome::Failed("TypeError: nope".to_owned())
        );
    }

    #[test]
    fn our_own_gap_is_not_a_failure_and_names_the_construct() {
        let error = Err(Error::NotImplemented(
            "not implemented yet: t.js:1:1: a for loop is not supported yet. See https://github.com/tamnd/katsu/milestones".to_owned(),
        ));
        assert_eq!(
            judge(&error, None),
            Outcome::Unsupported("a for loop is not supported yet".to_owned())
        );
    }

    #[test]
    fn a_negative_parse_test_we_reject_for_our_own_reasons_is_not_a_pass() {
        // The test this whole file exists for. Roughly a tenth of the suite expects to be
        // rejected, and we currently reject most of everything, so crediting these would turn the
        // pass rate into a measurement of how much is unbuilt.
        let error = Err(Error::NotImplemented(
            "not implemented yet: t.js:1:1: a class is not supported yet".to_owned(),
        ));
        let outcome = judge(&error, Some(&negative(Phase::Parse, "SyntaxError")));
        assert_ne!(outcome, Outcome::Passed);
        assert_eq!(outcome.label(), "unsupported");
    }

    #[test]
    fn a_negative_parse_test_we_reject_as_invalid_passes() {
        let error = Err(Error::Syntax("t.js: Unexpected token".to_owned()));
        assert_eq!(
            judge(&error, Some(&negative(Phase::Parse, "SyntaxError"))),
            Outcome::Passed
        );
    }

    #[test]
    fn a_negative_test_that_runs_cleanly_fails_and_says_what_was_expected() {
        let outcome = judge(&Ok(()), Some(&negative(Phase::Runtime, "TypeError")));
        assert_eq!(outcome.label(), "failed");
        assert!(
            outcome.reason().contains("TypeError"),
            "{}",
            outcome.reason()
        );
    }

    #[test]
    fn a_negative_runtime_test_matches_on_the_constructor_and_not_the_message() {
        // The message is not specified and every engine words it differently, so matching on it
        // would make the suite a test of our phrasing.
        let error = Err(Error::Uncaught(
            "TypeError: Cannot read properties of null (reading 'x')".to_owned(),
        ));
        assert_eq!(
            judge(&error, Some(&negative(Phase::Runtime, "TypeError"))),
            Outcome::Passed
        );
        assert_eq!(
            judge(&error, Some(&negative(Phase::Runtime, "RangeError"))).label(),
            "failed"
        );
    }

    #[test]
    fn a_runtime_failure_where_a_parse_failure_was_expected_is_a_failure() {
        // Both are rejections and they are not interchangeable. A file that is supposed to be
        // refused before it runs and instead runs partway has already done something observable.
        let error = Err(Error::Uncaught("SyntaxError: bad".to_owned()));
        assert_eq!(
            judge(&error, Some(&negative(Phase::Parse, "SyntaxError"))).label(),
            "failed"
        );
    }

    #[test]
    fn rejecting_a_valid_program_is_a_failure_that_says_so() {
        let error = Err(Error::Syntax("t.js: Unexpected token".to_owned()));
        let outcome = judge(&error, None);
        assert_eq!(outcome.label(), "failed");
        assert!(outcome.reason().starts_with("rejected valid source"));
    }

    #[test]
    fn the_constructor_comes_off_the_front_of_a_thrown_message() {
        assert_eq!(thrown_kind("TypeError: nope"), "TypeError");
        assert_eq!(thrown_kind("no colon here"), "no colon here");
    }

    #[test]
    fn the_work_list_counts_constructs_and_not_file_paths() {
        // Otherwise the histogram has one row per test rather than one row per missing feature,
        // which makes it a list of fifty thousand ones and useless as a work list.
        let a = construct(
            "not implemented yet: a/b/c.js:1:1: a for loop is not supported yet. See https://github.com/tamnd/katsu/milestones",
        );
        let b = construct(
            "not implemented yet: x/y/z.js:9:4: a for loop is not supported yet. See https://github.com/tamnd/katsu/milestones",
        );
        assert_eq!(a, b);
        assert_eq!(a, "a for loop is not supported yet");
    }
}
