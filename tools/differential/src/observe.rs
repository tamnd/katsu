//! What an engine did with a program, in a form two engines can be compared on.
//!
//! The hard part of a differential harness is not running two engines. It is deciding what counts
//! as the same answer, because two correct engines produce different bytes for the same program all
//! the time. An error message is the clearest case: the specification says an implementation throws
//! a `TypeError` and says nothing whatsoever about what the `TypeError` says, so comparing the text
//! would report a disagreement on every single throwing program and the harness would be useless
//! from its first run. What is comparable is the constructor name, so that is what gets kept.
//!
//! # Not implemented is not a disagreement
//!
//! Most generated programs will, for a long while, stop at something this engine has not built yet.
//! That is not katsu disagreeing with node, it is katsu not having an opinion. Those are their own
//! variant and their own line in the summary, so the run reports "we could not answer" separately
//! from "we answered differently", and a growing number of them reads as a work list rather than as
//! a growing pile of bugs.

use std::fmt;

use katsu_runtime::Error;

/// What one engine did with one program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// It ran to the end, and this is everything it printed.
    Printed(String),
    /// An exception reached the top. Only the constructor name is kept, because the message is
    /// unspecified and every engine writes its own.
    Threw(String),
    /// It refused the source before running any of it.
    Rejected(String),
    /// It reached a construct this build has not implemented. Not an answer, and not a bug.
    Unsupported(String),
    /// It stopped for a reason no JavaScript program could have caused: a timeout, a panic, a
    /// subprocess that would not start.
    Broke(String),
}

impl Observation {
    /// The short word for a summary line.
    pub(crate) const fn label(&self) -> &'static str {
        match *self {
            Observation::Printed(_) => "printed",
            Observation::Threw(_) => "threw",
            Observation::Rejected(_) => "rejected",
            Observation::Unsupported(_) => "unsupported",
            Observation::Broke(_) => "broke",
        }
    }

    /// Whether this observation says anything about correctness at all.
    ///
    /// An unimplemented construct and a broken oracle are both absences of an answer. Comparing
    /// them to anything would manufacture a disagreement out of work not yet done, which is the one
    /// way a fuzzer becomes something people stop reading.
    pub(crate) const fn is_answer(&self) -> bool {
        matches!(
            *self,
            Observation::Printed(_) | Observation::Threw(_) | Observation::Rejected(_)
        )
    }

    /// Turn what the embedding API returned into an observation.
    ///
    /// `printed` is what the recorder collected, which is only meaningful when nothing went wrong,
    /// since a program that threw halfway through printed a prefix that the other engine may well
    /// have printed too before throwing at a different point.
    pub(crate) fn from_result(result: &Result<(), Error>, printed: String) -> Observation {
        match result {
            Ok(()) => Observation::Printed(printed),
            Err(Error::Uncaught(text)) => Observation::Threw(kind(text).to_owned()),
            Err(Error::Syntax(text)) => Observation::Rejected(text.clone()),
            Err(Error::NotImplemented(text)) => Observation::Unsupported(construct(text)),
            Err(Error::Fatal(text)) => Observation::Broke(text.clone()),
        }
    }
}

impl fmt::Display for Observation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Observation::Printed(ref text) => write!(formatter, "printed {text:?}"),
            Observation::Threw(ref name) => write!(formatter, "threw {name}"),
            Observation::Rejected(ref why) => write!(formatter, "rejected the source: {why}"),
            Observation::Unsupported(ref what) => write!(formatter, "does not implement {what}"),
            Observation::Broke(ref why) => write!(formatter, "broke: {why}"),
        }
    }
}

/// How two observations relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Both engines answered, and gave the same answer.
    Agree,
    /// Both engines answered, and the answers differ. This is the one worth reporting.
    Differ,
    /// At least one of them did not answer, so there is nothing to compare.
    Untested,
}

/// Compare what two engines did.
pub(crate) fn compare(ours: &Observation, theirs: &Observation) -> Verdict {
    if !ours.is_answer() || !theirs.is_answer() {
        return Verdict::Untested;
    }
    let same = match (ours, theirs) {
        // Both refused the source, which is agreement. The text is not compared for the same
        // reason a thrown message is not: the standard says a syntax error is thrown and says
        // nothing at all about what it says, and node's version starts with the path of the
        // temporary file it was handed, so comparing the text reports a divergence on every
        // program either engine rejects. That was five of the first seven findings.
        (Observation::Rejected(_), Observation::Rejected(_)) => true,
        (left, right) => left == right,
    };
    if same {
        Verdict::Agree
    } else {
        Verdict::Differ
    }
}

/// The constructor name out of an error message.
///
/// `TypeError: x is not a function` is a disagreement with node only if node threw something other
/// than a `TypeError`. The text after the colon is deliberately unspecified by the standard, so
/// keeping it would turn every throwing program into a false report.
pub(crate) fn kind(text: &str) -> &str {
    let head = text.split_once(':').map_or(text, |(name, _)| name).trim();
    // Guard against a message with a colon in it that is not an error name at all, which would
    // otherwise become its own bogus error kind that never matches anything.
    if head.ends_with("Error") {
        head
    } else {
        "Error"
    }
}

/// Find the error name in whatever an engine wrote to standard error.
///
/// Node prints the source line, a caret, a blank line and then `TypeError: message`, followed by a
/// stack. Scanning for the first line that starts with an error name is what survives node changing
/// the decoration around it, which it has done more than once.
pub(crate) fn kind_in(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        let line = line.trim();
        let Some((head, _)) = line.split_once(':') else {
            continue;
        };
        if head.ends_with("Error") && !head.contains(' ') && !head.contains('/') {
            return Some(head.to_owned());
        }
    }
    None
}

/// The construct out of a not implemented message, so the summary counts constructs and not files.
fn construct(text: &str) -> String {
    let tail = text.rsplit_once(": ").map_or(text, |(_, tail)| tail);
    tail.trim_end_matches(" See https://github.com/tamnd/katsu/milestones")
        .trim_end()
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use katsu_runtime::Error;

    use super::{Observation, Verdict, compare, kind, kind_in};

    #[test]
    fn the_same_output_agrees() {
        let ours = Observation::Printed("1\n".to_owned());
        let theirs = Observation::Printed("1\n".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Agree);
    }

    #[test]
    fn different_output_differs() {
        let ours = Observation::Printed("0.1\n".to_owned());
        let theirs = Observation::Printed("0.30000000000000004\n".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Differ);
    }

    #[test]
    fn two_engines_throwing_the_same_kind_agree_despite_different_messages() {
        // The reason the comparison is on the constructor name. Node says "x is not a function"
        // and we say something else, and the standard has an opinion about neither. Comparing the
        // text would report a disagreement on every throwing program from the first run onward.
        let ours = Observation::from_result(
            &Err(Error::Uncaught("TypeError: not callable".to_owned())),
            String::new(),
        );
        let theirs = Observation::Threw("TypeError".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Agree);
    }

    #[test]
    fn throwing_different_kinds_differs() {
        let ours = Observation::Threw("TypeError".to_owned());
        let theirs = Observation::Threw("RangeError".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Differ);
    }

    #[test]
    fn an_unimplemented_construct_is_not_a_disagreement() {
        // The single most important line in the file. Without it every program that reaches
        // something we have not built reads as a bug, and the report becomes a list of the work
        // remaining wearing the word "divergence", which nobody would keep reading.
        let ours = Observation::from_result(
            &Err(Error::NotImplemented(
                "not implemented yet: t.js:1:1: a for loop is not supported yet. See https://github.com/tamnd/katsu/milestones".to_owned(),
            )),
            String::new(),
        );
        assert!(
            matches!(ours, Observation::Unsupported(ref what) if what == "a for loop is not supported yet")
        );
        let theirs = Observation::Printed("1\n".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Untested);
    }

    #[test]
    fn a_broken_oracle_is_not_a_disagreement_either() {
        // If node is not installed, every program would look like a divergence and the run would
        // report thousands of bugs that are one missing binary.
        let ours = Observation::Printed("1\n".to_owned());
        let theirs = Observation::Broke("node is not on the path".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Untested);
    }

    #[test]
    fn two_engines_rejecting_the_same_source_agree_whatever_they_said_about_it() {
        // Node's refusal begins with the path of the temporary file it was given, so this can never
        // match on text and comparing on text made every rejected program a false report.
        let ours = Observation::Rejected("Cannot assign to this expression".to_owned());
        let theirs = Observation::Rejected("/tmp/case.js:1".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Agree);
    }

    #[test]
    fn one_engine_rejecting_what_the_other_ran_is_a_disagreement() {
        // Worth its own case because it is the shape of the seven Annex B failures test262 found,
        // where our parser calls an early error on something node accepts and runs.
        let ours = Observation::Rejected("invalid assignment target".to_owned());
        let theirs = Observation::Printed("1\n".to_owned());
        assert_eq!(compare(&ours, &theirs), Verdict::Differ);
    }

    #[test]
    fn an_error_name_is_taken_off_the_front_of_a_message() {
        assert_eq!(kind("TypeError: x is not a function"), "TypeError");
        // A bare name with no message is still a name, which is what a rethrown error looks like.
        assert_eq!(kind("RangeError"), "RangeError");
        // A message with a colon that is not an error name must not become its own error kind.
        assert_eq!(kind("something: went wrong"), "Error");
    }

    #[test]
    fn an_error_name_is_found_in_what_node_wrote() {
        let stderr = "/tmp/case.js:3\nconsole.log(x.y);\n              ^\n\n\
                      TypeError: Cannot read properties of undefined\n    at Object.<anonymous>\n";
        assert_eq!(kind_in(stderr), Some("TypeError".to_owned()));
    }

    #[test]
    fn the_path_line_is_not_mistaken_for_an_error_name() {
        // `/tmp/case.js:3` splits on a colon just as happily as an error does.
        assert_eq!(kind_in("/tmp/case.js:3\nsomething\n"), None);
    }
}
