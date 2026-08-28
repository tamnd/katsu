//! The things a generated program is run against.
//!
//! An oracle answers one question: what did this engine do with this source. There are two of them
//! today. `Katsu` runs the program in process, which is fast enough that the harness spends its
//! time generating rather than waiting. `Node` runs it in a subprocess against the reference
//! implementation, which is the whole point of the exercise, since compatibility with node is the
//! goal and node is where the answer lives.
//!
//! # Why a missing node is loud
//!
//! If `node` is not on the path, the tempting behaviour is to fall back to comparing katsu against
//! itself and report that everything agreed. That is worse than doing nothing, because it produces
//! a green run that means nothing, and green runs that mean nothing are how a test suite stops
//! being read. So a missing node is reported once, at startup, in the words "not on the path", and
//! the run continues in the only mode that is still honest without it: checking that the engine
//! agrees with itself, which catches nondeterminism and nothing else, and says so.
//!
//! # Why the katsu oracle is a fresh runtime every time
//!
//! A program can assign to a global. Reusing one runtime would let program forty one change what
//! program forty two computes, and the resulting divergence would reproduce for whoever ran the
//! whole corpus and not for whoever ran the one seed, which is the least useful bug report there
//! is.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use katsu_runtime::{Recorder, Runtime};

use crate::observe::{Observation, kind_in};

/// How long any one program gets before it is treated as hung.
///
/// The generator only emits loops with literal bounds, so nothing it produces should come close.
/// Anything that does is a bug in the engine worth reporting as one, which is why a timeout is its
/// own observation and not silently a failure to run.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

/// Anything that can say what it did with a program.
pub(crate) trait Oracle {
    /// What this engine is called in a report.
    fn name(&self) -> &str;

    /// Run one program.
    fn observe(&mut self, source: &str) -> Observation;
}

/// Katsu itself, in process.
#[derive(Debug, Default)]
pub(crate) struct Katsu;

impl Oracle for Katsu {
    fn name(&self) -> &'static str {
        "katsu"
    }

    fn observe(&mut self, source: &str) -> Observation {
        let Ok(mut runtime) = Runtime::new() else {
            return Observation::Broke("cannot create a runtime".to_owned());
        };
        let recorder = Recorder::new();
        runtime.set_output(Box::new(recorder.clone()));

        // A thread holding the interrupt rather than a check in the loop here, because the eval
        // call does not return until the program does. It is asked to stop at the next back edge,
        // which is the only place the interpreter looks, so a program hung in straight line code
        // would still hang. That is spec 5.6's trade and this harness inherits it.
        //
        // The flag is what keeps this from being one sleeping thread per program for the whole run.
        // A watchdog that slept out its full timeout would leave five seconds' worth of threads
        // alive at all times, and a corpus run is tens of thousands of programs.
        let interrupt = runtime.interrupt();
        let done = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let watchdog = std::thread::spawn({
            let done = Arc::clone(&done);
            let fired = Arc::clone(&fired);
            move || {
                let deadline = Instant::now() + TIMEOUT;
                while !done.load(Ordering::Acquire) {
                    if Instant::now() >= deadline {
                        fired.store(true, Ordering::Release);
                        interrupt.request();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        });

        let result = runtime.eval("generated.js", source).map(|_| ());
        done.store(true, Ordering::Release);
        let _ = watchdog.join();

        if fired.load(Ordering::Acquire) {
            // Reported as its own thing rather than as whatever error the interrupt produced,
            // because "this took longer than five seconds" is the finding and the `Fatal` the
            // interpreter returns is only how the finding was delivered.
            return Observation::Broke(format!("did not finish in {TIMEOUT:?}"));
        }
        Observation::from_result(&result, recorder.text())
    }
}

/// Node, in a subprocess.
#[derive(Debug)]
pub(crate) struct Node {
    /// What to invoke, so somebody can point this at a specific build.
    program: String,
    /// Where the source goes. One file, rewritten per program, rather than one file per program,
    /// because a corpus run is tens of thousands of programs and that is tens of thousands of
    /// inodes for no gain.
    scratch: PathBuf,
}

impl Node {
    /// Check that node is there and usable, and say what it is.
    ///
    /// # Errors
    ///
    /// Returns the reason if node cannot be run at all, which the caller reports rather than works
    /// around.
    pub(crate) fn find(program: &str, scratch: PathBuf) -> Result<(Node, String), String> {
        let output = Command::new(program)
            .arg("--version")
            .output()
            .map_err(|error| format!("cannot run {program}: {error}"))?;
        if !output.status.success() {
            return Err(format!("{program} --version failed"));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((
            Node {
                program: program.to_owned(),
                scratch,
            },
            version,
        ))
    }
}

impl Oracle for Node {
    fn name(&self) -> &'static str {
        "node"
    }

    fn observe(&mut self, source: &str) -> Observation {
        if let Err(error) = write_source(&self.scratch, source) {
            return Observation::Broke(format!("cannot write {}: {error}", self.scratch.display()));
        }

        let child = Command::new(&self.program)
            .arg(&self.scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => return Observation::Broke(format!("cannot start node: {error}")),
        };

        let deadline = Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => return Observation::Broke(format!("cannot wait for node: {error}")),
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Observation::Broke(format!("node did not finish in {TIMEOUT:?}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        let Ok(output) = child.wait_with_output() else {
            return Observation::Broke("cannot collect node's output".to_owned());
        };
        let printed = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Observation::Printed(printed);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        match kind_in(&stderr) {
            // A SyntaxError from node is a refusal to run the program rather than an exception the
            // program threw, and our own engine reports those two through different variants, so
            // flattening them here would report a disagreement on wording alone.
            Some(name) if name == "SyntaxError" => {
                Observation::Rejected(first_line(&stderr).to_owned())
            }
            Some(name) => Observation::Threw(name),
            None => Observation::Broke(format!(
                "node exited {} with no error name: {}",
                output.status,
                first_line(&stderr)
            )),
        }
    }
}

/// Write the source out, creating the parent directory if it is not there.
fn write_source(path: &std::path::Path, source: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(source.as_bytes())?;
    file.flush()
}

/// The first line of a message, for a report that stays one line per program.
fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::{Katsu, Node, Oracle, first_line};
    use crate::observe::{Observation, Verdict, compare};

    #[test]
    fn katsu_reports_what_a_program_printed() {
        let mut oracle = Katsu;
        assert_eq!(
            oracle.observe("console.log(1 + 1);"),
            Observation::Printed("2\n".to_owned())
        );
    }

    #[test]
    fn katsu_reports_a_refusal_apart_from_an_unimplemented_construct() {
        // The distinction the conformance runner needed and this one needs for the same reason: a
        // gap of ours must not read as node and katsu disagreeing about the grammar.
        let mut oracle = Katsu;
        assert!(matches!(oracle.observe("let ="), Observation::Rejected(_)));
    }

    #[test]
    fn one_program_cannot_change_what_the_next_one_computes() {
        // A fresh runtime per program, asserted rather than trusted, because the failure mode is a
        // divergence that reproduces for whoever runs the whole corpus and not for whoever runs the
        // one seed out of it.
        let mut oracle = Katsu;
        let first = oracle.observe("var leaked = 41; console.log(leaked);");
        assert_eq!(first, Observation::Printed("41\n".to_owned()));
        let second = oracle.observe("console.log(typeof leaked);");
        assert_eq!(second, Observation::Printed("undefined\n".to_owned()));
    }

    #[test]
    fn a_missing_node_is_an_error_rather_than_a_silent_fallback() {
        // Falling back to comparing katsu with katsu would produce a green run that means nothing,
        // which is worse than a red one.
        let result = Node::find(
            "definitely-not-a-real-node-binary",
            std::env::temp_dir().join("katsu-differential-never.js"),
        );
        assert!(result.is_err(), "a missing binary reported as present");
    }

    #[test]
    fn katsu_agrees_with_node_on_the_number_formatting_edge_cases() {
        // Skipped rather than failed when node is absent, because a developer without node still
        // needs the rest of the suite to run, and the harness itself says so at startup.
        let scratch = std::env::temp_dir().join("katsu-differential-test.js");
        let Ok((mut node, _)) = Node::find("node", scratch) else {
            return;
        };
        let mut katsu = Katsu;
        for source in [
            "console.log(0.1 + 0.2);",
            "console.log(1e21);",
            "console.log(1e-7);",
            "console.log(-0);",
            "console.log(1 / 0, -1 / 0, 0 / 0);",
            "console.log(2147483648 | 0);",
            "console.log(\"10\" < \"9\");",
            "console.log(9007199254740993);",
        ] {
            let ours = katsu.observe(source);
            let theirs = node.observe(source);
            assert_eq!(
                compare(&ours, &theirs),
                Verdict::Agree,
                "{source}\n  katsu {ours}\n  node  {theirs}"
            );
        }
    }

    #[test]
    fn a_message_becomes_one_line() {
        assert_eq!(first_line("\n\n  boom\nand more\n"), "boom");
        assert_eq!(first_line(""), "");
    }
}
