//! Runs the same program in two places and reports where the answers differ.
//!
//! The design goal is that every optimization the compilation tiers ever make is a claim that
//! something faster is still equivalent to the interpreter, and this harness is how that claim gets
//! checked continuously rather than argued about. See `spec/14-quality-bar.md`.
//!
//! # What it compares today, which is not that
//!
//! There is one tier. Comparing it against itself would catch nondeterminism and nothing else, so
//! rather than wait for M6 to have a second tier, the reference implementation is used as the
//! oracle in the meantime: katsu against node, on generated programs. That is a strictly harder
//! question than tier against tier, it is the question the project is actually trying to answer,
//! and the machinery it needs is the same machinery. When the tiers arrive they become two more
//! oracles behind the same trait and the generator, the shrinker and the report do not change.
//!
//! When node is not installed the run does not quietly pass. It says so, and falls back to running
//! the interpreter twice, which checks determinism and is described as exactly that.
//!
//! # Why generated programs and not only a corpus
//!
//! A corpus contains the bugs somebody already thought of. The literals in the generator are chosen
//! from the places two implementations of ECMAScript are known to stop agreeing, and the structure
//! around them is random, which is how a harness finds the combination nobody thought to write
//! down. Both are run, because a corpus is also the regression suite for everything the generator
//! found once.

mod generate;
mod observe;
mod oracle;
mod random;
mod shrink;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;

use generate::Program;
use observe::{Observation, Verdict, compare};
use oracle::{Katsu, Node, Oracle};

/// Compare katsu against a reference implementation on generated and corpus programs.
#[derive(Debug, Parser)]
#[command(name = "differential", about, long_about = None)]
struct Args {
    /// Directory of JavaScript files to run before the generated ones.
    #[arg(long, default_value = "tools/differential/corpus")]
    corpus: PathBuf,

    /// How many programs to generate.
    #[arg(long, default_value_t = 1000)]
    count: u64,

    /// How many statements each generated program gets before its final print.
    #[arg(long, default_value_t = 8)]
    statements: usize,

    /// Seed for anything random, so a failure is reproducible from the log line.
    ///
    /// Program `n` of a run comes from `seed + n`, so a single seed in a report is enough to get
    /// the same program back without rerunning everything before it.
    #[arg(long, default_value_t = 0)]
    seed: u64,

    /// Which node to compare against.
    #[arg(long, default_value = "node")]
    node: String,

    /// Run exactly one seed and print the program, for looking at a report by hand.
    #[arg(long)]
    only: Option<u64>,

    /// Force a deoptimization every N instructions, to exercise the deopt paths.
    ///
    /// Deoptimization bugs hide because deoptimization is rare in normal execution. Making it
    /// common is the only way to find them. Accepted now and unused until there is a tier to
    /// deoptimize from, which is M6.
    #[arg(long)]
    deopt_every: Option<u32>,

    /// How many divergences to print in full before the report turns into a count.
    #[arg(long, default_value_t = 10)]
    max_reports: usize,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("differential: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Everything, returning whether the two engines agreed everywhere they both had an answer.
fn run(args: &Args) -> Result<bool> {
    if let Some(every) = args.deopt_every {
        eprintln!(
            "differential: --deopt-every {every} was given and there is no tier to deoptimize \
             from yet, so it does nothing. That arrives with M6."
        );
    }

    let mut katsu = Katsu;
    let scratch =
        std::env::temp_dir().join(format!("katsu-differential-{}.js", std::process::id()));
    let (mut reference, mode): (Box<dyn Oracle>, &str) =
        match Node::find(&args.node, scratch.clone()) {
            Ok((node, version)) => {
                eprintln!("differential: comparing against node {version}");
                (Box::new(node), "compatibility")
            }
            Err(reason) => {
                // Loud, and named for what it actually becomes. A harness that silently compares an
                // engine with itself and reports that everything agreed is worse than one that does
                // not run, because somebody trusts the green.
                eprintln!(
                    "differential: {reason}. Falling back to running the interpreter twice, which \
                     checks that it is deterministic and checks nothing about node compatibility."
                );
                (Box::new(Katsu), "determinism")
            }
        };

    let mut tally = Tally::default();
    let started = Instant::now();

    if let Some(seed) = args.only {
        let program = generate::program(seed, args.statements);
        println!("{}", program.source());
        let ours = katsu.observe(&program.source());
        let theirs = reference.observe(&program.source());
        println!("katsu: {ours}");
        println!("{}: {theirs}", reference.name());
        return Ok(compare(&ours, &theirs) != Verdict::Differ);
    }

    for program in corpus(&args.corpus)? {
        check(&program, &mut katsu, reference.as_mut(), &mut tally, args);
    }
    let from_corpus = tally.total;

    for offset in 0..args.count {
        let program = generate::program(args.seed.wrapping_add(offset), args.statements);
        check(&program, &mut katsu, reference.as_mut(), &mut tally, args);
    }

    let _ = std::fs::remove_file(&scratch);
    report(
        &tally,
        mode,
        from_corpus,
        args.count,
        started.elapsed().as_secs_f64(),
    );
    Ok(tally.differ == 0)
}

/// Run one program through both oracles and record what happened.
fn check(
    program: &Program,
    katsu: &mut Katsu,
    reference: &mut dyn Oracle,
    tally: &mut Tally,
    args: &Args,
) {
    let source = program.source();
    let ours = katsu.observe(&source);
    let theirs = reference.observe(&source);
    tally.total += 1;

    match compare(&ours, &theirs) {
        Verdict::Agree => tally.agree += 1,
        Verdict::Untested => {
            tally.untested += 1;
            // Which side had no answer, and why. Without this the untested count is one number that
            // could be a thousand unimplemented constructs or a thousand crashed subprocesses, and
            // those two want completely different things done about them.
            let name = reference.name().to_owned();
            if !ours.is_answer() {
                *tally
                    .blocked
                    .entry(format!("katsu {}", ours.label()))
                    .or_default() += 1;
            }
            if !theirs.is_answer() {
                *tally
                    .blocked
                    .entry(format!("{name} {}", theirs.label()))
                    .or_default() += 1;
            }
            // Only ours, because a construct node does not implement is not a thing that exists.
            if let Observation::Unsupported(ref what) = ours {
                *tally.unsupported.entry(what.clone()).or_default() += 1;
            }
        }
        Verdict::Differ => {
            tally.differ += 1;
            if tally.differ > args.max_reports {
                return;
            }
            // Shrunk before printing rather than after, because a divergence nobody reads is a
            // divergence nobody fixes, and forty lines of generated source is not read.
            let smallest = shrink::shrink(program, |candidate| {
                let source = candidate.source();
                compare(&katsu.observe(&source), &reference.observe(&source)) == Verdict::Differ
            });
            let name = reference.name().to_owned();
            println!("\ndifferential: divergence, seed {}", program.seed);
            for statement in &smallest.statements {
                println!("  {statement}");
            }
            println!("  katsu {}", katsu.observe(&smallest.source()));
            println!("  {name} {}", reference.observe(&smallest.source()));
            println!("  reproduce with: --only {}", program.seed);
        }
    }
}

/// Everything the run learned, in the order the report wants it.
#[derive(Debug, Default)]
struct Tally {
    total: usize,
    agree: usize,
    differ: usize,
    untested: usize,
    /// Which side had no answer and in what way, so the untested count can be read.
    blocked: BTreeMap<String, usize>,
    /// What stopped us, most common first once sorted. The work list, on the same principle as the
    /// conformance runner's: derived from what actually ran rather than from opinion.
    unsupported: BTreeMap<String, usize>,
}

/// Read the corpus, which is allowed to be missing.
///
/// A corpus file is split into lines so the shrinker can work on it the same way it works on a
/// generated program. That is not a general JavaScript statement splitter and it does not need to
/// be, because a removal that produces something invalid simply fails the predicate and is kept.
fn corpus(directory: &Path) -> Result<Vec<Program>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut programs = Vec::new();
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(directory)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| path.extension().is_some_and(|kind| kind == "js"))
        .collect();
    paths.sort();

    for path in paths {
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        programs.push(Program {
            seed: 0,
            statements: source.lines().map(str::to_owned).collect(),
        });
    }
    Ok(programs)
}

/// How many constructs to list before the work list turns into a number.
const LISTED: usize = 10;

/// Print what the run found.
fn report(tally: &Tally, mode: &str, from_corpus: usize, generated: u64, seconds: f64) {
    eprintln!(
        "\ndifferential: {} programs in {seconds:.1}s, {from_corpus} from the corpus and \
         {generated} generated, checking {mode}",
        tally.total
    );
    eprintln!("  agreed      {}", tally.agree);
    eprintln!("  differed    {}", tally.differ);
    eprintln!(
        "  untested    {} (one side had no answer, which is a gap and not a bug)",
        tally.untested
    );
    for (who, count) in &tally.blocked {
        eprintln!("    {count:>6}  {who}");
    }

    if !tally.unsupported.is_empty() {
        let mut rows: Vec<(&String, &usize)> = tally.unsupported.iter().collect();
        rows.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        eprintln!("\nWhat stopped us, most common first. This is the work list.");
        for (what, count) in rows.iter().take(LISTED) {
            eprintln!("  {count:>6}  {what}");
        }
        if rows.len() > LISTED {
            eprintln!("  and {} more", rows.len() - LISTED);
        }
    }

    if tally.differ == 0 {
        eprintln!("\ndifferential: no divergences.");
    } else {
        eprintln!(
            "\ndifferential: {} divergence(s). Each one is a program where we and the reference \
             both had an answer and the answers were not the same.",
            tally.differ
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Tally, corpus, report};

    #[test]
    fn a_missing_corpus_is_not_an_error() {
        // A developer who has not created the directory should still get a generated run rather
        // than a failure about a directory nothing requires.
        let programs = corpus(std::path::Path::new(
            "tools/differential/definitely-not-here",
        ));
        assert!(programs.is_ok());
        assert!(programs.unwrap().is_empty());
    }

    #[test]
    fn the_corpus_on_disk_is_readable_and_not_empty() {
        // The corpus is the regression suite for everything found once, so an empty one is a
        // silently disabled test rather than a tidy repository.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let programs = corpus(&root).expect("the corpus should be readable");
        assert!(
            !programs.is_empty(),
            "no corpus files at {}",
            root.display()
        );
        for program in &programs {
            assert!(!program.statements.is_empty(), "an empty corpus file");
        }
    }

    #[test]
    fn a_report_with_nothing_in_it_does_not_panic() {
        report(&Tally::default(), "determinism", 0, 0, 0.0);
    }
}
