//! The embedding API. This is the stable public surface.
//!
//! Everything below this crate is internal and may change in any release. This crate is
//! the contract, with full semver and deprecation before removal, and the Node compatible
//! layer sits on top of it rather than beside it. If the Node layer ever needs something
//! this API cannot express, that is a bug in this API, and we would rather find it that way
//! than have an embedder find it. See `spec/16-package-layout.md`.

use std::fmt;

use katsu_vm::{Interpreter, RuntimeError};

pub use katsu_vm::{
    Discard, Interrupt, Isolate, Output, Recorder, Standard, Stream, Value, start_clock,
};

/// Anything that went wrong that a caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The source did not parse or could not be lowered.
    #[error("syntax error: {0}")]
    Syntax(String),
    /// A JavaScript exception reached the top of the stack.
    #[error("uncaught exception: {0}")]
    Uncaught(String),
    /// The feature exists in the design but is not implemented in this build yet.
    ///
    /// A real variant rather than a `todo!`, so that an unfinished path returns an error a
    /// caller can handle instead of aborting the process.
    #[error("not implemented yet: {0}. See https://github.com/tamnd/katsu/milestones")]
    NotImplemented(String),
    /// The runtime cannot continue, and no JavaScript program could have caught it either.
    ///
    /// Running out of memory or out of address space, or an execution that something asked to
    /// stop. Separate from [`Error::Uncaught`] because an uncaught exception is a thing the
    /// program did and this is a thing that happened to it.
    #[error("fatal: {0}")]
    Fatal(String),
}

impl From<RuntimeError> for Error {
    fn from(error: RuntimeError) -> Error {
        match error {
            RuntimeError::NotImplemented(_) | RuntimeError::Unsupported(_) => {
                Error::NotImplemented(error.to_string())
            }
            RuntimeError::OutOfMemory | RuntimeError::Interrupted => {
                Error::Fatal(error.to_string())
            }
            // Everything a `try` in the program could have caught and nothing did. An error the
            // engine raised says what Node says word for word, and a value the program threw has
            // already been rendered by the interpreter that owned it, because a value cannot be
            // read once it is outside the heap it lives in.
            other => Error::Uncaught(other.to_string()),
        }
    }
}

/// One embedded katsu runtime.
///
/// Owns an isolate and, when the `jit` feature is on, the compilation tiers for it. Not
/// `Sync`, by design: two threads never touch one heap. To run JavaScript on another
/// thread, make another `Runtime`.
pub struct Runtime {
    interpreter: Interpreter,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("isolate", self.isolate())
            .field("jit", &jit_enabled())
            .finish()
    }
}

impl Runtime {
    /// Create a runtime with default options.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Fatal`] if the heap or the stack cannot reserve their address space, which
    /// on a sixty four bit system means the process is already in trouble. It is fallible rather
    /// than panicking because an embedder that has run out of address space usually wants to fail
    /// the request rather than take the whole host process down with it.
    pub fn new() -> Result<Runtime, Error> {
        let mut interpreter =
            Interpreter::new().map_err(|error| Error::Fatal(error.to_string()))?;
        // The globals a program is entitled to assume are there. It is here rather than inside
        // `Interpreter::new` because the interpreter is the machine and this is the standard library
        // it happens to be started with. An embedder that wants a bare machine wants the
        // interpreter, not this.
        katsu_builtins::install_globals(&mut interpreter)?;
        katsu_builtins::install_console(&mut interpreter)?;
        katsu_builtins::install_performance(&mut interpreter)?;
        katsu_builtins::install_string(&mut interpreter)?;
        katsu_builtins::install_json(&mut interpreter)?;
        Ok(Runtime { interpreter })
    }

    /// Send everything this runtime prints somewhere other than the process's own streams.
    ///
    /// Returns the sink that was there, so a caller can put it back when it is done. A
    /// [`Recorder`] reads back what a program printed, which is how a test asserts on output, and
    /// [`Discard`] throws it away, which is what an embedder running untrusted code wants.
    pub fn set_output(&mut self, output: Box<dyn Output>) -> Box<dyn Output> {
        self.interpreter.set_output(output)
    }

    /// Evaluate a source string and return the value its top level returns.
    ///
    /// Which is `undefined` today, for every program. A script's completion value, the thing that
    /// makes `eval("1 + 1")` answer two rather than nothing, is a property of the statements the
    /// program is made of rather than of the expressions in it, and tracking it correctly means
    /// threading it through every statement form in lowering. That is its own piece of work and it
    /// is not this one. Until it lands, a program is observed through what it prints.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Syntax`] if the source does not parse, [`Error::Uncaught`] if the program
    /// throws, and [`Error::NotImplemented`] if it reaches a construct this build does not run yet.
    pub fn eval(&mut self, path: &str, source: &str) -> Result<Value, Error> {
        // Two different failures wear the same shape here and they are not the same thing. A file
        // that is not valid JavaScript is the program's mistake. A file that is valid JavaScript
        // using something we have not built is ours. Collapsing them was fine while nothing read
        // the difference, and stopped being fine the moment a conformance runner did: a negative
        // test expects to be rejected, so every gap of ours would have counted as a pass.
        let blueprint = katsu_vm::compile(path, source).map_err(|error| {
            if error.is_not_implemented() {
                Error::NotImplemented(error.to_string())
            } else {
                Error::Syntax(error.to_string())
            }
        })?;
        Ok(self.interpreter.run(&blueprint)?)
    }

    /// Render a value as text, the way `console.log` prints it at the top level.
    ///
    /// A value is only meaningful alongside the runtime that produced it, because a string is an
    /// address into that runtime's heap. This is how an embedder reads one without being handed the
    /// heap itself.
    #[must_use]
    pub fn display(&self, value: Value) -> String {
        self.interpreter.display(value)
    }

    /// The isolate this runtime owns.
    #[must_use]
    pub const fn isolate(&self) -> &Isolate {
        self.interpreter.isolate()
    }

    /// A handle another thread can use to stop whatever this runtime is running.
    ///
    /// The interpreter checks it on every loop back edge, so a program stuck in a loop stops and a
    /// program stuck in straight line code does not, there being no back edge to check at. That is
    /// the trade spec 5.6 makes deliberately, since a check on every instruction would cost every
    /// instruction and straight line code is finite by construction.
    ///
    /// What this is for is a caller running JavaScript it did not write, which in practice means a
    /// conformance suite and an embedder with untrusted input. Both need to survive a program that
    /// never returns.
    #[must_use]
    pub fn interrupt(&self) -> Interrupt {
        self.interpreter.interrupt()
    }
}

/// Whether the compilation tiers are compiled into this build.
#[must_use]
pub const fn jit_enabled() -> bool {
    cfg!(feature = "jit")
}

#[cfg(test)]
mod tests {
    use super::{Discard, Error, Recorder, Runtime};

    fn runtime() -> Runtime {
        Runtime::new().expect("should start")
    }

    #[test]
    fn a_syntax_error_comes_back_as_a_syntax_error() {
        let error = runtime()
            .eval("bad.js", "const = ;")
            .expect_err("should not parse");
        assert!(matches!(error, Error::Syntax(_)), "got {error:?}");
    }

    #[test]
    fn source_goes_in_and_it_runs() {
        // Text at one end and execution at the other, with nothing in between an embedder has to
        // know about. This is the first release where the second half of that sentence is true.
        let mut runtime = runtime();
        let value = runtime
            .eval(
                "ok.js",
                "let name = 'katsu'; let greeting = 'hello ' + name;",
            )
            .expect("should run");
        // Undefined rather than the greeting, because a script has no completion value here yet.
        // The comment on `eval` says why, and this asserts it rather than leaving it to be
        // discovered.
        assert_eq!(runtime.display(value), "undefined");
    }

    #[test]
    fn a_program_that_throws_reports_what_node_reports() {
        let mut runtime = runtime();
        let error = runtime
            .eval("bad.js", "const x = 1; x = 2;")
            .expect_err("should throw");
        assert_eq!(
            error.to_string(),
            "uncaught exception: TypeError: Assignment to constant variable."
        );
    }

    #[test]
    fn something_this_build_cannot_run_says_so_and_names_it() {
        let mut runtime = runtime();
        // Named property access runs now, so the opcode this stops at has moved along again, to a
        // key computed at run time. It is still the case being checked: something the frontend can
        // lower and the loop cannot run is named rather than being guessed at or crashed on.
        let error = runtime
            .eval("ok.js", "console['log']")
            .expect_err("a computed key is not implemented yet");
        assert!(matches!(error, Error::NotImplemented(_)), "got {error:?}");
    }

    #[test]
    fn a_name_that_was_never_bound_reads_as_a_reference_error() {
        let mut runtime = runtime();
        let error = runtime
            .eval("bad.js", "missing.log(1)")
            .expect_err("nothing is bound to missing");
        assert_eq!(
            error.to_string(),
            "uncaught exception: ReferenceError: missing is not defined"
        );
    }

    #[test]
    fn a_program_can_print() {
        // The first release where a program can be observed doing something. Until scripts have
        // completion values this is the only way to see what one computed, which is why it is the
        // milestone line it is.
        let mut runtime = runtime();
        let recorder = Recorder::new();
        runtime.set_output(Box::new(recorder.clone()));
        runtime
            .eval("hello.js", "console.log('hello ' + 'katsu')")
            .expect("should run");
        assert_eq!(recorder.text(), "hello katsu\n");
    }

    #[test]
    fn an_exception_unwinds_out_of_the_calls_it_was_thrown_in() {
        // The reason a handler table is searched across frames rather than inside one. Node prints
        // exactly this, and the frames in between are gone by the time the handler runs.
        let mut runtime = runtime();
        let recorder = Recorder::new();
        runtime.set_output(Box::new(recorder.clone()));
        runtime
            .eval(
                "deep.js",
                "function bottom() { throw 'from the bottom'; }
                 function middle() { bottom(); return 'not reached'; }
                 try { middle(); } catch (e) { console.log(e); }",
            )
            .expect("the handler catches it");
        assert_eq!(recorder.text(), "from the bottom\n");
    }

    #[test]
    fn a_caught_engine_error_carries_the_name_and_the_message_node_uses() {
        // The message is word for word what Node prints, because the whole point of matching it is
        // that somebody searching the web for their error finds an answer that applies.
        let mut runtime = runtime();
        let recorder = Recorder::new();
        runtime.set_output(Box::new(recorder.clone()));
        runtime
            .eval(
                "err.js",
                "try { null.x; } catch (e) { console.log(e.name, e.message); }",
            )
            .expect("the handler catches it");
        assert_eq!(
            recorder.text(),
            "TypeError Cannot read properties of null (reading 'x')\n"
        );
    }

    #[test]
    fn an_uncaught_throw_reports_the_value_and_not_a_message_about_it() {
        // Node's last line for `throw 'boom'` is the word on its own, and the CLI prints an
        // uncaught exception bare for exactly that reason, so a script grepping our output finds
        // what it would have found from Node.
        let mut runtime = runtime();
        let error = runtime
            .eval("bad.js", "throw 'boom';")
            .expect_err("nothing catches it");
        assert!(matches!(error, Error::Uncaught(_)), "got {error:?}");
        assert_eq!(error.to_string(), "uncaught exception: boom");
    }

    #[test]
    fn what_a_program_prints_can_be_thrown_away() {
        let mut runtime = runtime();
        runtime.set_output(Box::new(Discard));
        runtime
            .eval("quiet.js", "console.log('nobody hears this')")
            .expect("should run");
    }

    #[test]
    fn the_sink_that_was_there_comes_back_so_it_can_be_put_back() {
        let mut runtime = runtime();
        let recorder = Recorder::new();
        let standard = runtime.set_output(Box::new(recorder.clone()));
        runtime.eval("t.js", "console.log(1)").expect("should run");
        runtime.set_output(standard);
        runtime.eval("t.js", "console.log(2)").expect("should run");
        // Only the first line, because the second went back out to the process.
        assert_eq!(recorder.text(), "1\n");
    }
}
