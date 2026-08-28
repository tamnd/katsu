//! Runs test262 against katsu and compares the result to a checked in expectations file.
//!
//! The expectations file is the point. A raw pass rate tells you nothing on a project that
//! is going to spend two years below 100%, but a diff against the last known result tells
//! you exactly what you broke. A test that starts passing has to be committed too, which is
//! what stops the file from quietly accumulating permission to fail.
//! See `spec/14-quality-bar.md`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

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
}

fn main() -> ExitCode {
    let args = Args::parse();

    if !args.suite.exists() {
        eprintln!(
            "test262-runner: no suite at {}. Clone it with:\n  \
             git clone --depth 1 https://github.com/tc39/test262 {}",
            args.suite.display(),
            args.suite.display()
        );
        return ExitCode::FAILURE;
    }

    eprintln!(
        "test262-runner: the runner is milestone M1 and the engine cannot execute yet, so \
         there is nothing to report. Expectations file: {}. Bless mode: {}.",
        args.expectations.display(),
        args.bless
    );
    ExitCode::SUCCESS
}
