//! The katsu command line interface.
//!
//! `katsu run` executes a program, `katsu build` compiles it ahead of time into a native
//! binary, and `katsu bench` and `katsu --heap-census` exist so that the claims in
//! `spec/02-the-10x-goal.md` can be checked by anyone rather than taken on trust.
//!
//! # Which stream a message goes to and which code it exits with
//!
//! A program's own output goes to standard output and everything else goes to standard error, so
//! that `katsu run app.js > out.txt` puts the program's output in the file and the reason it stopped
//! on the terminal. That is what Node does and a script that pipes us is written against it.
//!
//! Exit one for anything that went wrong, which is also what Node does for an uncaught exception.
//! Distinct codes per failure kind is a thing to add when there is a reason to tell them apart from
//! a shell, and inventing them before then means picking numbers somebody will come to depend on.
//!
//! An uncaught exception prints as Node prints it, which means the error and its message with no
//! prefix in front. Anything that is the command's own problem rather than the program's, such as a
//! file that will not open, is prefixed with `katsu:` so the two are not confused. Node has a stack
//! trace under its error and we do not, because a frame needs a source span and a function name and
//! those arrive with the same work that makes `x is not a function` name `x`.

mod check;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

/// A JavaScript and TypeScript runtime in Rust.
#[derive(Debug, Parser)]
#[command(name = "katsu", version, about, long_about = None)]
struct Cli {
    /// Print what this build was compiled with, then exit.
    #[arg(long)]
    build_info: bool,

    /// Report where memory is going, line by line, and exit.
    ///
    /// This is the command behind the idle memory budget. It is a user facing subcommand
    /// rather than a debug flag because a budget nobody can inspect is a budget that rots.
    #[arg(long)]
    heap_census: bool,

    /// Force execution to stay in a given tier. For debugging and for reproducing bugs.
    ///
    /// Goes before the subcommand, as `katsu --tier=baseline run app.js`.
    #[arg(long, value_enum)]
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

/// Why the process stopped, which decides both the exit code and which stream the message goes to.
enum Stop {
    /// The program threw and nothing caught it. Printed the way Node prints it.
    Uncaught(String),
    /// The command could not do what it was asked. Printed with a `katsu:` prefix.
    Failed(String),
    /// The compiler this command shelled out to has already said its piece.
    ///
    /// Its exit code is forwarded and nothing more is printed, because printing our own summary on
    /// top of `tsc`'s output would be noise in front of the thing somebody actually needs to read.
    Forwarded(u8),
}

fn main() -> ExitCode {
    // First, before anything else in the process does any work. This is the origin that
    // `performance.timeOrigin` reports and that `performance.now()` counts from, so every line
    // after it is startup that a program timing itself can see. Moving it below the argument parse
    // or below the logger would quietly exclude that work from our own numbers, which is the kind of
    // flattering measurement this project exists to not make.
    katsu_runtime::start_clock();

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
        Err(Stop::Uncaught(message)) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        Err(Stop::Failed(message)) => {
            eprintln!("katsu: {message}");
            ExitCode::FAILURE
        }
        Err(Stop::Forwarded(code)) => ExitCode::from(code),
    }
}

fn run(command: Command) -> Result<(), Stop> {
    match command {
        Command::Run { script, args } => execute(&script, &args),
        Command::Build {
            entry,
            out,
            profile,
        } => {
            let _ = (out, profile);
            Err(Stop::Failed(format!(
                "`katsu build` is milestone M9 and is not implemented yet. {} parsed fine, which \
                 is as far as this build gets. Progress: \
                 https://github.com/tamnd/katsu/milestones",
                entry.display()
            )))
        }
        Command::Check { entry } => type_check(&entry),
    }
}

/// `katsu run`.
fn execute(script: &std::path::Path, args: &[String]) -> Result<(), Stop> {
    let path = script.display().to_string();
    let source = std::fs::read_to_string(script)
        .map_err(|error| Stop::Failed(format!("cannot read {path}: {error}")))?;

    // Said out loud rather than dropped on the floor. `process.argv` needs a process object, which
    // is M2, and a program silently seeing an empty argument list is a much worse way to find that
    // out than a line on standard error.
    if !args.is_empty() {
        eprintln!(
            "katsu: ignoring {} argument(s) after the script, because process.argv needs a process \
             object and that is milestone M2",
            args.len()
        );
    }

    let mut runtime = katsu_runtime::Runtime::new().map_err(failure)?;
    // The value the top level produces is dropped, because running a file is not the same as
    // evaluating an expression and Node does not print it either. A program says what it has to say
    // through what it prints.
    runtime.eval(&path, &source).map(|_| ()).map_err(failure)
}

/// Decide whether a failure was the program's or ours, which decides how it prints.
///
/// The rule is whether a Node process given the same file would have failed the same way. A program
/// that throws and a program that does not parse are both the program's problem, and both print
/// bare, because a script that greps our output for `TypeError` should find it where Node puts it.
/// Something we have not built yet, or an isolate that could not be created, is ours, and wearing
/// the `katsu:` prefix is how somebody tells at a glance that their file is not the thing to fix.
///
/// An uncaught exception drops the words in front of it and prints as its error name and message,
/// because that is the line Node ends with and it is the line people match on. A syntax error keeps
/// its wording, since ours is not yet shaped like Node's, which names `SyntaxError` and puts a caret
/// under the offending token. Getting there needs the source span work that is also what makes a
/// stack frame nameable, and guessing at the shape before then would put the caret in the wrong
/// column, which is worse than not drawing one.
fn failure(error: katsu_runtime::Error) -> Stop {
    match error {
        katsu_runtime::Error::Uncaught(message) => Stop::Uncaught(message),
        syntax @ katsu_runtime::Error::Syntax(_) => Stop::Uncaught(syntax.to_string()),
        other => Stop::Failed(other.to_string()),
    }
}

/// `katsu check`.
fn type_check(entry: &std::path::Path) -> Result<(), Stop> {
    if !entry.is_file() {
        return Err(Stop::Failed(format!("cannot read {}", entry.display())));
    }

    let found = check::find(entry, std::env::var_os("KATSU_TSC"), tsc_on_path())
        .ok_or_else(|| Stop::Failed(check::nowhere(entry)))?;
    let project = check::project_of(entry);
    let arguments = check::arguments(entry, project.as_deref());
    tracing::debug!(program = ?found.program, found = ?found.found, ?arguments, "checking");

    let status = std::process::Command::new(&found.program)
        .args(&arguments)
        .status()
        .map_err(|error| {
            Stop::Failed(format!("cannot run {}: {error}", found.program.display()))
        })?;
    if status.success() {
        return Ok(());
    }
    // A compiler that was killed by a signal has no code to forward, and one is invented rather
    // than reporting success, because a check that did not finish is not a check that passed.
    Err(Stop::Forwarded(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1),
    ))
}

/// Whether a bare `tsc` would run, which is the last of the three places `check` looks.
///
/// Asking the operating system to look rather than reading `PATH` and joining it ourselves, because
/// the rules are not the same on every platform and `PATHEXT` on Windows is a whole thing. Running
/// it with `--version` is cheap and it is the same question `Command` will ask a moment later.
fn tsc_on_path() -> bool {
    std::process::Command::new(if cfg!(windows) { "tsc.cmd" } else { "tsc" })
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
    println!("target: {}", target());
    println!("pointer width: {} bits", usize::BITS);
}

/// What this binary was built for, as much of a target triple as the standard library knows.
///
/// Assembled from `std::env::consts` rather than from a build script that captures `TARGET`,
/// because a build script exists to be run and this is three constants the compiler already has.
/// It is not literally the triple, since the standard library reports the operating system and the
/// environment separately and does not report the vendor at all, and printing something shaped like
/// a triple that is not one would be worse than printing what is actually known.
fn target() -> String {
    let (arch, os, family) = (
        std::env::consts::ARCH,
        std::env::consts::OS,
        std::env::consts::FAMILY,
    );
    if family.is_empty() {
        format!("{arch}-{os}")
    } else {
        format!("{arch}-{os} ({family})")
    }
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

#[cfg(test)]
mod tests {
    use super::{Cli, target};
    use clap::Parser;

    #[test]
    fn the_target_names_the_architecture_and_the_operating_system() {
        // Not asserting which one, because the test runs on three of them. What is asserted is the
        // shape, since a `--build-info` that printed `aarch64-` would be a broken line nobody would
        // notice until somebody pasted it into a bug report.
        let text = target();
        let (arch, rest) = text.split_once('-').expect("should have both halves");
        assert!(!arch.is_empty(), "got {text}");
        assert!(!rest.is_empty(), "got {text}");
        assert_eq!(arch, std::env::consts::ARCH);
    }

    #[test]
    fn the_arguments_after_a_script_belong_to_the_script() {
        // Including the ones that look like ours. `katsu run app.js --build-info` has to pass
        // `--build-info` to the program rather than printing our own build information, or a
        // program can never be given a flag we happen to share a name with.
        let cli = Cli::try_parse_from(["katsu", "run", "app.js", "--build-info", "-x"])
            .expect("should parse");
        let Some(super::Command::Run { script, args }) = cli.command else {
            panic!("should be a run");
        };
        assert_eq!(script.to_str(), Some("app.js"));
        assert_eq!(args, vec!["--build-info".to_owned(), "-x".to_owned()]);
        assert!(!cli.build_info);
    }

    #[test]
    fn build_info_needs_no_subcommand() {
        let cli = Cli::try_parse_from(["katsu", "--build-info"]).expect("should parse");
        assert!(cli.build_info);
        assert!(cli.command.is_none());
    }

    #[test]
    fn check_takes_one_entry_and_nothing_else() {
        let cli = Cli::try_parse_from(["katsu", "check", "src/app.ts"]).expect("should parse");
        let Some(super::Command::Check { entry }) = cli.command else {
            panic!("should be a check");
        };
        assert_eq!(entry.to_str(), Some("src/app.ts"));
    }
}
