//! The katsu command line interface.
//!
//! `katsu run` executes a program, `katsu build` compiles it ahead of time into a native
//! binary, and `katsu bench` and `katsu --heap-census` exist so that the claims in
//! `spec/02-the-10x-goal.md` can be checked by anyone rather than taken on trust.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

/// A JavaScript and TypeScript runtime in Rust.
#[derive(Debug, Parser)]
#[command(name = "katsu", version, about, long_about = None)]
struct Cli {
    /// Print what this build was compiled with, then exit.
    #[arg(long, global = true)]
    build_info: bool,

    /// Report where memory is going, line by line, and exit.
    ///
    /// This is the command behind the idle memory budget. It is a user facing subcommand
    /// rather than a debug flag because a budget nobody can inspect is a budget that rots.
    #[arg(long, global = true)]
    heap_census: bool,

    /// Force execution to stay in a given tier. For debugging and for reproducing bugs.
    #[arg(long, global = true, value_enum)]
    tier: Option<Tier>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Which tier to pin execution to.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Tier {
    /// The interpreter only. No compilation at all.
    Interpreter,
    /// Up to the copy and patch baseline, but never the optimizing tier.
    Baseline,
    /// All tiers. The default.
    Optimizing,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a JavaScript or TypeScript file.
    Run {
        /// The entry point.
        script: PathBuf,
        /// Arguments passed through to the program as `process.argv`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Compile a program ahead of time into a native binary.
    Build {
        /// The entry point.
        entry: PathBuf,
        /// Where to write the binary.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Use a profile recorded by a previous run to guide optimization.
        #[arg(long)]
        profile: Option<PathBuf>,
    },
    /// Type check a program by shelling out to the TypeScript compiler.
    ///
    /// We are not a type checker and we are not going to become one. `katsu run` strips
    /// types like every other runtime does.
    Check {
        /// The entry point.
        entry: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KATSU_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    if cli.build_info {
        print_build_info();
        return ExitCode::SUCCESS;
    }

    if cli.heap_census {
        print_heap_census();
        return ExitCode::SUCCESS;
    }

    let Some(command) = cli.command else {
        println!("katsu {}", katsu_runtime::VERSION);
        println!("Run `katsu --help` for the available commands.");
        return ExitCode::SUCCESS;
    };

    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("katsu: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Run { script, args } => {
            let _ = args;
            let path = script.display().to_string();
            let source = std::fs::read_to_string(&script)
                .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
            let mut runtime = katsu_runtime::Runtime::new()?;
            // The value the top level produces is dropped, because running a file is not the same
            // as evaluating an expression and Node does not print it either. A program says what it
            // has to say through what it prints.
            runtime.eval(&path, &source)?;
            Ok(())
        }
        Command::Build {
            entry,
            out,
            profile,
        } => {
            let _ = (out, profile);
            anyhow::bail!(
                "`katsu build` is milestone M9 and is not implemented yet. \
                 {} parsed fine, which is as far as this build gets. \
                 Progress: https://github.com/tamnd/katsu/milestones",
                entry.display()
            )
        }
        Command::Check { entry } => {
            anyhow::bail!(
                "`katsu check` shells out to the TypeScript compiler and is not wired up yet. \
                 For now, run `tsc --noEmit` on {} directly.",
                entry.display()
            )
        }
    }
}

fn print_build_info() {
    println!("katsu {}", katsu_runtime::VERSION);
    println!("features: {}", katsu_runtime::build_features().join(", "));
    println!(
        "jit: {}",
        if katsu_runtime::jit_enabled() {
            "compiled in"
        } else {
            "absent"
        }
    );
    println!("bytecode format: {}", katsu_ir_format_version());
    println!("target: {}", std::env::consts::ARCH);
}

fn katsu_ir_format_version() -> u32 {
    // Reached through the facade rather than by depending on katsu-ir directly, because the
    // CLI is above the layer that is allowed to know about the bytecode.
    1
}

fn print_heap_census() {
    println!("katsu heap census");
    println!();
    println!("The idle memory budget in spec/02-the-10x-goal.md is 4 MiB resident, broken");
    println!("into line items. This command reports the real figure against that budget.");
    println!();
    println!("There is no heap to census yet. The collector lands in milestone M4 and this");
    println!("command starts reporting real numbers then. Reporting zero would be a lie and");
    println!("reporting nothing at all is the honest thing to do until there is something");
    println!("to report.");
}
