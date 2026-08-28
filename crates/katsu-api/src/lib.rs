//! The embedding API. This is the stable public surface.
//!
//! Everything below this crate is internal and may change in any release. This crate is
//! the contract, with full semver and deprecation before removal, and the Node compatible
//! layer sits on top of it rather than beside it. If the Node layer ever needs something
//! this API cannot express, that is a bug in this API, and we would rather find it that way
//! than have an embedder find it. See `spec/16-package-layout.md`.

use std::fmt;

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
    NotImplemented(&'static str),
}

/// One embedded katsu runtime.
///
/// Owns an isolate and, when the `jit` feature is on, the compilation tiers for it. Not
/// `Sync`, by design: two threads never touch one heap. To run JavaScript on another
/// thread, make another `Runtime`.
pub struct Runtime {
    isolate: Isolate,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("isolate", &self.isolate)
            .field("jit", &jit_enabled())
            .finish()
    }
}

impl Runtime {
    /// Create a runtime with default options.
    #[must_use]
    pub fn new() -> Self {
        Self {
            isolate: Isolate::new(),
        }
    }

    /// Evaluate a source string and return its completion value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Syntax`] if the source does not parse, and
    /// [`Error::NotImplemented`] while execution is still being built out in M0 and M1.
    pub fn eval(&mut self, path: &str, source: &str) -> Result<Value, Error> {
        let blueprint =
            katsu_vm::compile(path, source).map_err(|e| Error::Syntax(e.to_string()))?;
        let _ = blueprint;
        Err(Error::NotImplemented(
            "bytecode execution, tracked by milestone M0",
        ))
    }

    /// The isolate this runtime owns.
    #[must_use]
    pub fn isolate(&self) -> &Isolate {
        &self.isolate
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn a_syntax_error_comes_back_as_a_syntax_error() {
        let mut runtime = Runtime::new();
        let error = runtime
            .eval("bad.js", "const = ;")
            .expect_err("should not parse");
        assert!(matches!(error, Error::Syntax(_)), "got {error:?}");
    }

    #[test]
    fn valid_source_parses_and_then_says_honestly_that_it_cannot_run_it_yet() {
        let mut runtime = Runtime::new();
        let error = runtime
            .eval("ok.js", "1 + 1")
            .expect_err("M0 is not finished");
        assert!(matches!(error, Error::NotImplemented(_)), "got {error:?}");
    }
}
