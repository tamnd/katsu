//! What the `katsu` binary does when you run it, asserted by running it.
//!
//! The unit tests in `main.rs` cover argument parsing and the ones in `check.rs` cover finding a
//! compiler, and neither of those can tell you that the binary prints to the right stream or exits
//! with the right code. Those are the two things every script that wraps a runtime depends on and
//! the two things a refactor can break without a single unit test noticing, so they are asserted
//! here against the real binary that `cargo` just built.
//!
//! `CARGO_BIN_EXE_katsu` is the path to it, which cargo sets for integration tests, so there is no
//! guessing at a target directory and no chance of testing a stale binary from a previous build.

use std::env::consts as env;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

/// Run the binary and hand back what it did, with output captured rather than inherited.
fn katsu(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_katsu"))
        .args(args)
        .output()
        .expect("should run the binary cargo just built")
}

fn out(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("output should be text")
}

fn err(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("output should be text")
}

/// Write a program into a temporary directory that goes away when the test ends.
fn script(name: &str, source: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("should make a temporary directory");
    let path = directory.path().join(name);
    std::fs::write(&path, source).expect("should write the program");
    (directory, path)
}

#[test]
fn a_program_runs_and_what_it_prints_goes_to_standard_output() {
    let (_directory, path) = script("hello.js", "console.log('hello from katsu');\n");
    let output = katsu(&["run", path.to_str().expect("path should be text")]);
    assert!(output.status.success(), "stderr: {}", err(&output));
    assert_eq!(out(&output), "hello from katsu\n");
    assert_eq!(err(&output), "");
}

#[test]
fn the_value_at_the_top_level_is_not_printed() {
    // Running a file is not evaluating an expression. Node prints nothing here and a program that
    // ends in a bare expression would otherwise print it, which would corrupt anything piping us.
    let (_directory, path) = script("quiet.js", "1 + 1;\n");
    let output = katsu(&["run", path.to_str().expect("path should be text")]);
    assert!(output.status.success(), "stderr: {}", err(&output));
    assert_eq!(out(&output), "");
}

#[test]
fn an_uncaught_exception_prints_bare_on_standard_error_and_exits_one() {
    // The shape matters as much as the text. A script that greps for `TypeError:` at the start of a
    // line finds it in Node, so it has to find it here, which means no prefix in front of it.
    let (_directory, path) = script("boom.js", "console.log('before');\nnull.x;\n");
    let output = katsu(&["run", path.to_str().expect("path should be text")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(out(&output), "before\n");
    assert_eq!(
        err(&output),
        "TypeError: Cannot read properties of null (reading 'x')\n"
    );
}

#[test]
fn a_file_that_will_not_open_is_our_problem_and_says_so() {
    // The other half of the rule. This one is not the program's fault, there is no program, so it
    // wears the prefix that says the thing to fix is the command line rather than the source.
    let output = katsu(&["run", "definitely/not/here.js"]);
    assert_eq!(output.status.code(), Some(1));
    let message = err(&output);
    assert!(message.starts_with("katsu: cannot read "), "got {message}");
    assert!(message.contains("definitely/not/here.js"), "got {message}");
}

#[test]
fn arguments_after_the_script_are_not_silently_swallowed() {
    // They cannot be delivered until there is a process object, and a program seeing an empty
    // argument list with no explanation is a much worse way to find that out than a line saying so.
    let (_directory, path) = script("args.js", "console.log('ran');\n");
    let output = katsu(&[
        "run",
        path.to_str().expect("path should be text"),
        "--flag",
        "x",
    ]);
    assert!(output.status.success(), "stderr: {}", err(&output));
    assert_eq!(out(&output), "ran\n");
    let message = err(&output);
    assert!(message.contains("ignoring 2 argument"), "got {message}");
}

#[test]
fn build_info_says_what_this_build_is() {
    let output = katsu(&["--build-info"]);
    assert!(output.status.success(), "stderr: {}", err(&output));
    let text = out(&output);
    for line in [
        "katsu ",
        "features:",
        "jit:",
        "bytecode format:",
        "target:",
        "pointer width:",
    ] {
        assert!(text.contains(line), "{line} missing from:\n{text}");
    }
    // The target is the line somebody pastes into a bug report, so it has to name the machine
    // rather than half of it.
    assert!(
        text.contains(&format!("target: {}-{}", env::ARCH, env::OS)),
        "got:\n{text}"
    );
}

#[test]
fn the_heap_census_admits_there_is_no_heap_yet() {
    // Reporting zero would be a lie. This asserts the honesty rather than the numbers, and it will
    // be rewritten into an assertion about numbers when the collector lands in M4.
    let output = katsu(&["--heap-census"]);
    assert!(output.status.success(), "stderr: {}", err(&output));
    assert!(
        out(&output).contains("no heap to census yet"),
        "got:\n{}",
        out(&output)
    );
}

#[test]
fn check_with_no_compiler_anywhere_names_all_three_places_it_looked() {
    // KATSU_TSC is set to something that does not exist rather than unset, because the machine
    // running this test may well have a real tsc installed and the point is the message when there
    // is none. An empty PATH takes the third place away.
    let (_directory, path) = script("app.ts", "const x: number = 1;\n");
    let output = Command::new(env!("CARGO_BIN_EXE_katsu"))
        .args(["check", path.to_str().expect("path should be text")])
        .env_remove("KATSU_TSC")
        .env("PATH", "")
        .output()
        .expect("should run the binary");
    assert_eq!(output.status.code(), Some(1));
    let message = err(&output);
    assert!(
        message.starts_with("katsu: no TypeScript compiler"),
        "got {message}"
    );
    assert!(message.contains("KATSU_TSC"), "got {message}");
    assert!(message.contains("node_modules/.bin"), "got {message}");
    assert!(message.contains("tsc on the path"), "got {message}");
}

#[test]
fn check_on_a_file_that_is_not_there_does_not_start_a_compiler() {
    let output = katsu(&["check", "definitely/not/here.ts"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        err(&output).starts_with("katsu: cannot read "),
        "got {}",
        err(&output)
    );
}

#[cfg(unix)]
#[test]
fn check_forwards_the_compilers_exit_code_and_gets_out_of_the_way() {
    // A stand in compiler, because the real one is a 60 MB install this test has no business
    // requiring, and what is being asserted is our half of the contract: we run what we found, we
    // print nothing over the top of it, and the code it exits with is the code we exit with.
    let (directory, path) = script("app.ts", "const x: number = 1;\n");
    let fake = directory.path().join("fake-tsc");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho \"app.ts(1,7): error TS2322: nope\"\nexit 2\n",
    )
    .expect("should write");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("should chmod");

    let output = Command::new(env!("CARGO_BIN_EXE_katsu"))
        .args(["check", path.to_str().expect("path should be text")])
        .env("KATSU_TSC", &fake)
        .output()
        .expect("should run the binary");
    assert_eq!(output.status.code(), Some(2));
    assert!(out(&output).contains("TS2322"), "got:\n{}", out(&output));
    // Nothing of ours on top of what the compiler said, because a summary in front of the errors
    // somebody needs to read is noise.
    assert_eq!(err(&output), "");
}

#[cfg(unix)]
#[test]
fn a_check_that_passes_is_silent_and_exits_zero() {
    let (directory, path) = script("app.ts", "const x: number = 1;\n");
    let fake = directory.path().join("fake-tsc");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("should write");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("should chmod");

    let output = Command::new(env!("CARGO_BIN_EXE_katsu"))
        .args(["check", path.to_str().expect("path should be text")])
        .env("KATSU_TSC", &fake)
        .output()
        .expect("should run the binary");
    assert!(output.status.success(), "stderr: {}", err(&output));
    assert_eq!(out(&output), "");
    assert_eq!(err(&output), "");
}

#[test]
fn build_says_which_milestone_it_is_waiting_on() {
    // Not implemented is a fine answer. Pretending to build and producing nothing is not, and
    // neither is a panic, which is what this asserts is not happening.
    let (_directory, path) = script("app.js", "console.log(1);\n");
    let output = katsu(&["build", path.to_str().expect("path should be text")]);
    assert_eq!(output.status.code(), Some(1));
    let message = err(&output);
    assert!(message.starts_with("katsu: "), "got {message}");
    assert!(message.contains("M9"), "got {message}");
}

#[test]
fn no_subcommand_at_all_points_at_the_help() {
    let output = katsu(&[]);
    assert!(output.status.success(), "stderr: {}", err(&output));
    assert!(out(&output).contains("--help"), "got:\n{}", out(&output));
}
