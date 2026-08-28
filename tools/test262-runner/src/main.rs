//! Runs test262 against katsu and compares the result to a checked in expectations file.
//!
//! The expectations file is the point. A raw pass rate tells you nothing on a project that
//! is going to spend two years below 100%, but a diff against the last known result tells
//! you exactly what you broke. A test that starts passing has to be committed too, which is
//! what stops the file from quietly accumulating permission to fail.
//! See `spec/14-quality-bar.md`.
//!
//! # What the number means today
//!
//! Very little, and that is the honest state of it rather than a caveat to be embarrassed about.
//! Almost everything in the suite reaches a construct this build has not implemented, and those are
//! counted apart from failures because they are different questions: a failure is a bug and an
//! unsupported case is a piece of work. The run ends with a histogram of the constructs that
//! stopped it, most common first, which is the closest thing this project has to a work list
//! derived from evidence rather than from opinion.
//!
//! The report deliberately does not add those two numbers together. A combined figure would be
//! neither a bug count nor a work list, and it would improve every time we implemented anything,
//! which makes it exactly the sort of number a project quotes at itself.

mod expectations;
mod metadata;
mod outcome;
mod suite;
mod watchdog;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use katsu_runtime::{Discard, Runtime};
use rayon::prelude::*;

use expectations::{Expectations, Summary};
use outcome::Outcome;
use suite::{Case, Harness, Test};

/// Run the test262 conformance suite.
#[derive(Debug, Parser)]
#[command(name = "test262-runner", about, long_about = None)]
struct Args {
    /// Path to a checkout of tc39/test262.
    #[arg(long, default_value = "vendor/test262")]
    suite: PathBuf,

    /// Path to the expectations file.
    #[arg(long, default_value = "tools/test262-runner/expectations.json")]
    expectations: PathBuf,

    /// Rewrite the expectations file instead of failing on a difference.
    #[arg(long)]
    bless: bool,

    /// Only run tests whose path contains this text.
    ///
    /// For working on one area. A filtered run never touches the expectations file, because a
    /// passing set built from a tenth of the suite would delete the other nine tenths.
    #[arg(long)]
    filter: Option<String>,

    /// How many tests to run at once. Defaults to one per core.
    #[arg(long)]
    jobs: Option<usize>,

    /// How many reasons to list in the breakdown at the end.
    #[arg(long, default_value_t = 15)]
    top: usize,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("test262-runner: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Everything, returning whether the run met its expectations.
fn run(args: &Args) -> Result<bool> {
    if !args.suite.exists() {
        anyhow::bail!(
            "no suite at {}. Clone it with:\n  git clone --depth 1 https://github.com/tc39/test262 {}",
            args.suite.display(),
            args.suite.display()
        );
    }

    if let Some(jobs) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .context("cannot size the thread pool")?;
    }

    // Silenced because a run finds panics in the engine, and fifty thousand backtraces on stderr
    // buries the report they were supposed to lead to. The payload is caught and reported per case
    // instead, so nothing is lost, it just arrives counted rather than shouted.
    std::panic::set_hook(Box::new(|_| {}));

    let harness = Harness::load(&args.suite)?;
    if harness.is_empty() {
        anyhow::bail!(
            "{}/harness has no JavaScript in it, so every test would fail on its first line",
            args.suite.display()
        );
    }
    let paths = suite::find(&args.suite)?;
    let revision = suite::revision(&args.suite);
    eprintln!(
        "test262-runner: {} test files, {} harness files, from {} at {}",
        paths.len(),
        harness.len(),
        args.suite.display(),
        if revision.is_empty() {
            "an unknown revision"
        } else {
            &revision
        }
    );

    let watchdog = watchdog::Watchdog::new(Duration::from_secs(suite::TIMEOUT_SECONDS));
    let done = AtomicUsize::new(0);
    let started = Instant::now();

    let results: Vec<(String, Outcome)> = paths
        .par_iter()
        .filter(|path| match &args.filter {
            Some(text) => path.to_string_lossy().contains(text.as_str()),
            None => true,
        })
        .flat_map(|path| {
            let finished = done.fetch_add(1, Ordering::Relaxed);
            // Every few thousand, so a run that takes minutes says it is alive without the progress
            // line costing more than the work.
            if finished > 0 && finished.is_multiple_of(5_000) {
                eprintln!("test262-runner: {finished} files");
            }
            one_file(&args.suite, path, &harness, &watchdog)
        })
        .collect();

    drop(watchdog);
    report(&results, started.elapsed(), args.top);

    let summary = tally(&results);
    let passing: BTreeSet<String> = results
        .iter()
        .filter(|(_, outcome)| *outcome == Outcome::Passed)
        .map(|(name, _)| name.clone())
        .collect();

    if args.filter.is_some() {
        eprintln!("\ntest262-runner: filtered run, so the expectations file was not touched.");
        return Ok(true);
    }

    if args.bless {
        Expectations::from_run(revision, summary, passing).write(&args.expectations)?;
        eprintln!("\ntest262-runner: wrote {}", args.expectations.display());
        return Ok(true);
    }

    let file = Expectations::read(&args.expectations)?;
    if !file.suite.is_empty() && !revision.is_empty() && file.suite != revision {
        // A warning rather than a refusal, because comparing across revisions is a thing somebody
        // deliberately does when updating the suite. What it must not be is a thing that happens
        // silently, since the diff it produces is indistinguishable from a real regression.
        eprintln!(
            "\ntest262-runner: warning, {} was taken against suite {} and this checkout is at {}. \
             Tests added or renamed in between will show up below as if they had changed.",
            args.expectations.display(),
            file.suite,
            revision
        );
    }
    Ok(check(&file, &passing, &results, &args.expectations))
}

/// How many names to print before the list turns into a number.
const LISTED: usize = 40;

/// Compare against the file and say what to do about a difference.
fn check(
    file: &Expectations,
    passing: &BTreeSet<String>,
    results: &[(String, Outcome)],
    path: &Path,
) -> bool {
    let difference = file.compare(passing);
    if difference.is_empty() {
        eprintln!("\ntest262-runner: matches {}", path.display());
        return true;
    }

    if !difference.regressed.is_empty() {
        // What it does now, not just that it stopped. A name on its own sends whoever reads this
        // back to rerun the case by hand, and the answer is already in the results.
        let outcomes: HashMap<&str, &Outcome> = results
            .iter()
            .map(|(name, outcome)| (name.as_str(), outcome))
            .collect();
        eprintln!(
            "\ntest262-runner: {} case(s) stopped passing:",
            difference.regressed.len()
        );
        for name in difference.regressed.iter().take(LISTED) {
            match outcomes.get(name.as_str()) {
                Some(outcome) => {
                    eprintln!(
                        "  {name}\n    {} {}",
                        outcome.label(),
                        first_line(outcome.reason())
                    );
                }
                // Not in the results at all, which means the file is no longer being run: renamed,
                // deleted, or newly caught by a skip rule. Worth its own wording, because it is the
                // one kind of regression that is not a bug in the engine.
                None => eprintln!("  {name}\n    no longer run at all"),
            }
        }
        if difference.regressed.len() > LISTED {
            eprintln!("  and {} more", difference.regressed.len() - LISTED);
        }
    }
    if !difference.improved.is_empty() {
        eprintln!(
            "\ntest262-runner: {} case(s) started passing. Run with --bless and commit the result.",
            difference.improved.len()
        );
        for name in difference.improved.iter().take(LISTED) {
            eprintln!("  {name}");
        }
        if difference.improved.len() > LISTED {
            eprintln!("  and {} more", difference.improved.len() - LISTED);
        }
    }
    false
}

/// Read one file and run every case it turns into.
fn one_file(
    root: &Path,
    path: &Path,
    harness: &Harness,
    watchdog: &watchdog::Watchdog,
) -> Vec<(String, Outcome)> {
    let test = match suite::read(root, path) {
        Ok(Some(test)) => test,
        // No metadata block, so there are no instructions and running it would be a guess.
        Ok(None) => return vec![(name_of(root, path), Outcome::Skipped("no metadata block"))],
        // Almost always a file that is not valid UTF-8, of which the suite has a few on purpose.
        Err(_) => {
            return vec![(
                name_of(root, path),
                Outcome::Skipped("cannot be read as text"),
            )];
        }
    };

    if let Some(reason) = suite::skip_reason(&test.meta, &test.name, &test.source) {
        return vec![(test.name.clone(), Outcome::Skipped(reason))];
    }

    // A missing include is a broken checkout rather than a failed test, and saying so is more
    // useful than reporting fifty tests as failures.
    let Ok(cases) = test.cases(harness) else {
        return vec![(
            test.name.clone(),
            Outcome::Skipped("a harness file is missing"),
        )];
    };

    cases
        .into_iter()
        .map(|case| {
            let outcome = judge_one(&test, &case, watchdog);
            (case.name, outcome)
        })
        .collect()
}

/// Run one case in its own runtime and decide what happened.
///
/// A fresh runtime per case rather than one per worker thread. Reusing one would let a test that
/// assigns to a global change the answer of the next test on that thread, which produces failures
/// that depend on the order the files happened to be walked in and disappear when you rerun the one
/// that failed.
fn judge_one(test: &Test, case: &Case, watchdog: &watchdog::Watchdog) -> Outcome {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut runtime = Runtime::new()?;
        // Otherwise fifty thousand programs print to our stdout and the report is unreadable.
        runtime.set_output(Box::new(Discard));
        let interrupt = runtime.interrupt();
        let ticket = watchdog.watch(interrupt.clone());
        let result = runtime.eval(&test.name, &case.source).map(drop);
        drop(ticket);
        Ok::<_, katsu_runtime::Error>((result, interrupt.requested()))
    }));

    match caught {
        Ok(Ok((result, timed_out))) => {
            if timed_out {
                // Reported as its own thing rather than as whatever error the interrupt produced,
                // because "did not finish in five seconds" and "threw the wrong error" are not the
                // same finding and only one of them is about the test.
                return Outcome::Failed(format!(
                    "did not finish within {} seconds",
                    suite::TIMEOUT_SECONDS
                ));
            }
            outcome::judge(&result, test.meta.negative.as_ref())
        }
        // Starting a runtime failed, which means the machine is out of address space rather than
        // that the test did anything.
        Ok(Err(error)) => Outcome::Failed(format!("could not start a runtime: {error}")),
        Err(payload) => Outcome::Failed(format!("panicked: {}", panic_message(&payload))),
    }
}

/// The text out of a caught panic, for the cases where there is one.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "no message".to_owned()
    }
}

/// The name for a file we could not get as far as reading metadata out of.
fn name_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root.join("test"))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Count the outcomes.
fn tally(results: &[(String, Outcome)]) -> Summary {
    let mut summary = Summary {
        total: results.len(),
        ..Summary::default()
    };
    for (_, outcome) in results {
        match outcome {
            Outcome::Passed => summary.passed += 1,
            Outcome::Failed(_) => summary.failed += 1,
            Outcome::Unsupported(_) => summary.unsupported += 1,
            Outcome::Skipped(_) => summary.skipped += 1,
        }
    }
    summary
}

/// Print the counts and the two histograms.
fn report(results: &[(String, Outcome)], elapsed: Duration, top: usize) {
    let summary = tally(results);
    println!(
        "\ntest262, {} cases in {:.1}s",
        summary.total,
        elapsed.as_secs_f64()
    );
    println!("  passed       {}", summary.passed);
    println!("  failed       {}", summary.failed);
    println!("  unsupported  {}", summary.unsupported);
    println!("  skipped      {}", summary.skipped);
    println!(
        "  pass rate    {:.2}% of the {} attempted",
        summary.rate(),
        summary.total - summary.skipped
    );

    histogram(
        results,
        "Not implemented yet, most common first. This is the work list.",
        |outcome| matches!(outcome, Outcome::Unsupported(_)),
        top,
    );
    histogram(
        results,
        "Failures, most common first. These are bugs.",
        |outcome| matches!(outcome, Outcome::Failed(_)),
        top,
    );
    histogram(
        results,
        "Not run, most common first. Each of these is a milestone away.",
        |outcome| matches!(outcome, Outcome::Skipped(_)),
        top,
    );
}

/// One breakdown, counted by reason.
fn histogram(
    results: &[(String, Outcome)],
    title: &str,
    wanted: impl Fn(&Outcome) -> bool,
    top: usize,
) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, outcome) in results.iter().filter(|(_, outcome)| wanted(outcome)) {
        *counts.entry(outcome.reason()).or_default() += 1;
    }
    if counts.is_empty() {
        return;
    }

    let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
    // By count, then by reason, so two reasons with the same count do not swap places between runs
    // and turn a diff of two reports into noise.
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));

    println!("\n{title}");
    let shown = rows.len().min(top);
    for (reason, count) in &rows[..shown] {
        println!("{count:>8}  {}", first_line(reason));
    }
    if rows.len() > shown {
        println!("{:>8}  other reasons", rows.len() - shown);
    }
}

/// The first line of a reason, truncated, because a thrown message can be a paragraph.
fn first_line(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 110 {
        return line.to_owned();
    }
    let cut: String = line.chars().take(107).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Outcome, first_line, name_of, tally};

    #[test]
    fn the_counts_add_up_to_the_total() {
        let results = vec![
            ("a".to_owned(), Outcome::Passed),
            ("b".to_owned(), Outcome::Failed("x".to_owned())),
            ("c".to_owned(), Outcome::Unsupported("y".to_owned())),
            ("d".to_owned(), Outcome::Skipped("z")),
            ("e".to_owned(), Outcome::Passed),
        ];
        let summary = tally(&results);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.passed, 2);
        assert_eq!(
            summary.passed + summary.failed + summary.unsupported + summary.skipped,
            summary.total
        );
    }

    #[test]
    fn a_name_is_relative_to_the_test_directory_with_forward_slashes() {
        let name = name_of(
            std::path::Path::new("/tmp/test262"),
            std::path::Path::new("/tmp/test262/test/language/x.js"),
        );
        assert_eq!(name, "language/x.js");
    }

    #[test]
    fn a_reason_is_one_line_and_bounded() {
        // A thrown message can be a paragraph, and a histogram row that wraps six times stops being
        // a histogram.
        assert_eq!(first_line("first\nsecond"), "first");
        assert!(first_line(&"x".repeat(400)).chars().count() <= 110);
    }

    #[test]
    fn an_endless_loop_is_stopped_rather_than_hanging_the_run() {
        // The one thing about this runner that cannot be checked by reading it. A suite contains
        // programs that never return, and without this the answer arrives as a hung terminal.
        let test = super::Test {
            name: "loop.js".to_owned(),
            source: String::new(),
            meta: super::metadata::parse("/*---\ndescription: x\n---*/\n").expect("block"),
        };
        let case = super::Case {
            name: "loop.js".to_owned(),
            source: "while (true) {}".to_owned(),
        };
        let watchdog = super::watchdog::Watchdog::new(Duration::from_millis(200));
        let outcome = super::judge_one(&test, &case, &watchdog);
        // A `while` loop may well be unsupported before it is ever endless, and that is a perfectly
        // good outcome here. What is being asserted is that this returns at all.
        assert!(
            matches!(outcome, Outcome::Failed(_) | Outcome::Unsupported(_)),
            "got {outcome:?}"
        );
    }
}
