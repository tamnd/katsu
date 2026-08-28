//! The embedding API. This is the stable public surface.
//!
//! Everything below this crate is internal and may change in any release. This crate is
//! the contract, with full semver and deprecation before removal, and the Node compatible
//! layer sits on top of it rather than beside it. If the Node layer ever needs something
//! this API cannot express, that is a bug in this API, and we would rather find it that way
//! than have an embedder find it. See `spec/16-package-layout.md`.

use std::fmt;

use katsu_vm::{Interpreter, RuntimeError};

pub use katsu_vm::{Isolate, Value};

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
            RuntimeError::NotImplemented(_) => Error::NotImplemented(error.to_string()),
            RuntimeError::OutOfMemory | RuntimeError::Interrupted => {
                Error::Fatal(error.to_string())
            }
            // The three JavaScript exceptions, which carry the message Node prints and which a
            // `try` will catch once there is one.
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
        Ok(Runtime {
            interpreter: Interpreter::new().map_err(|error| Error::Fatal(error.to_string()))?,
        })
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
        let blueprint =
            katsu_vm::compile(path, source).map_err(|e| Error::Syntax(e.to_string()))?;
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
}

/// Whether the compilation tiers are compiled into this build.
#[must_use]
pub const fn jit_enabled() -> bool {
    cfg!(feature = "jit")
}

#[cfg(test)]
mod tests {
    use super::{Error, Runtime};

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
        let error = runtime
            .eval("ok.js", "console.log(1)")
            .expect_err("calls are not implemented yet");
        assert!(matches!(error, Error::NotImplemented(_)), "got {error:?}");
    }
}
