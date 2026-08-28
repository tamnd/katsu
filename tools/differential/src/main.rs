//! Runs the same program in every tier and fails if they disagree.
//!
//! The interpreter is the oracle. It is the simplest implementation, it is the one test262
//! is run against, and every optimization in the compilation tiers is a claim that
//! something faster is still equivalent to it. This harness is how that claim is checked
//! continuously rather than argued about. See `spec/14-quality-bar.md`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Run a corpus through every tier and compare the results.
#[derive(Debug, Parser)]
#[command(name = "differential", about, long_about = None)]
struct Args {
    /// Directory of JavaScript files to run.
    #[arg(long, default_value = "tools/differential/corpus")]
    corpus: PathBuf,

    /// Force a deoptimization every N instructions, to exercise the deopt paths.
    ///
    /// Deoptimization bugs hide because deoptimization is rare in normal execution. Making
    /// it common is the only way to find them.
    #[arg(long)]
    deopt_every: Option<u32>,

    /// Seed for anything random, so a failure is reproducible from the log line.
    #[arg(long, default_value_t = 0)]
    seed: u64,
}

fn main() -> ExitCode {
    let args = Args::parse();
    eprintln!(
        "differential: milestone M11. There is one tier so far, so there is nothing to \
         differ. Corpus: {}, deopt-every: {:?}, seed: {}.",
        args.corpus.display(),
        args.deopt_every,
        args.seed
    );
    ExitCode::SUCCESS
}
