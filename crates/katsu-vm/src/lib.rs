//! The interpreter, the object model, shapes, inline caches and isolates.
//!
//! This is the largest crate in the workspace and the one everything above it depends on.
//! See `spec/05-interpreter.md` for dispatch, `spec/07-object-model.md` for values and
//! shapes, and `spec/03-architecture.md` for why an isolate is `Send` but not `Sync`.

mod interpret;
mod number;
mod stack;
mod value;

use std::fmt;

use katsu_ir::FunctionBlueprint;

pub use interpret::{Interpreter, Interrupt, RuntimeError};
pub use stack::{Frame, Stack, StackError};
pub use value::Value;

/// Why a source file could not be turned into something executable.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    /// The source did not parse.
    #[error(transparent)]
    Parse(#[from] katsu_parse::ParseError),
}

/// Turn source text into a blueprint the interpreter can execute.
///
/// The layers above the VM go through here rather than calling the parser directly, so
/// that the frontend stays swappable. Open question Q9 asks whether oxc is the right
/// dependency, and an answer of no should not reach past this function.
///
/// # Errors
///
/// Returns [`CompileError::Parse`] if the source does not parse.
pub fn compile(path: &str, source: &str) -> Result<FunctionBlueprint, CompileError> {
    let module = katsu_parse::parse(path, source)?;
    Ok(module.top_level)
}

/// One independent JavaScript heap with its own object graph.
///
/// An isolate is `Send` so it can be moved to another thread, and deliberately not `Sync`,
/// so that two threads cannot touch one heap. That is what buys us a collector with no
/// read barriers and an object model with no locks, and it is the same trade every
/// production engine makes. See `spec/03-architecture.md`.
pub struct Isolate {
    heap_reserved: usize,
}

impl fmt::Debug for Isolate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Isolate")
            .field("heap_reserved", &self.heap_reserved)
            .finish()
    }
}

impl Isolate {
    /// Create an isolate with an empty heap.
    #[must_use]
    pub fn new() -> Self {
        Self { heap_reserved: 0 }
    }

    /// Bytes this isolate has reserved from the operating system.
    #[must_use]
    pub fn heap_reserved(&self) -> usize {
        self.heap_reserved
    }
}

impl Default for Isolate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Isolate;

    #[test]
    fn an_isolate_can_be_moved_to_another_thread() {
        let isolate = Isolate::new();
        let reserved = std::thread::spawn(move || isolate.heap_reserved())
            .join()
            .expect("thread should not panic");
        assert_eq!(reserved, 0);
    }
}
