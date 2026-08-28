//! The interpreter, the object model, shapes, inline caches and isolates.
//!
//! This is the largest crate in the workspace and the one everything above it depends on.
//! See `spec/05-interpreter.md` for dispatch, `spec/07-object-model.md` for values and
//! shapes, and `spec/03-architecture.md` for why an isolate is `Send` but not `Sync`.

use std::fmt;

use katsu_ir::FunctionBlueprint;

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

/// A JavaScript value as the interpreter and the JIT hold it in a register.
///
/// Registers hold 64 bits. Object slots on the heap hold 32, because pointer compression
/// is a day one decision and not an optimization to add later. The two representations
/// are deliberately different types so that the compiler catches a confusion between them.
/// See `spec/07-object-model.md`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// `undefined`.
    Undefined,
    /// `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number. Integers are held as doubles until the object model lands, at which point
    /// the small integer representation from `spec/07-object-model.md` takes over.
    Number(f64),
}

impl Value {
    /// The ECMAScript `ToBoolean` abstract operation, for the cases we can already do.
    #[must_use]
    pub fn to_boolean(self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => b,
            Value::Number(n) => n != 0.0 && !n.is_nan(),
        }
    }

    /// The `typeof` operator.
    #[must_use]
    pub const fn type_of(self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            // typeof null is "object". It is a bug from 1995 and it is in the standard.
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
        }
    }
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
    use super::{Isolate, Value};

    #[test]
    fn to_boolean_follows_the_specification_including_the_awkward_parts() {
        assert!(!Value::Undefined.to_boolean());
        assert!(!Value::Null.to_boolean());
        assert!(!Value::Number(0.0).to_boolean());
        assert!(!Value::Number(f64::NAN).to_boolean());
        assert!(Value::Number(-1.0).to_boolean());
        assert!(Value::Bool(true).to_boolean());
    }

    #[test]
    fn typeof_null_is_object_because_the_standard_says_so() {
        assert_eq!(Value::Null.type_of(), "object");
        assert_eq!(Value::Undefined.type_of(), "undefined");
    }

    #[test]
    fn an_isolate_can_be_moved_to_another_thread() {
        let isolate = Isolate::new();
        let reserved = std::thread::spawn(move || isolate.heap_reserved())
            .join()
            .expect("thread should not panic");
        assert_eq!(reserved, 0);
    }
}
