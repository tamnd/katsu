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

use katsu_gc::{Atom, AtomTable, BumpHeap, Cage, CageError, StringRef};
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
///
/// It owns the cage, the bump heap inside it and the atom table that interns strings into it.
/// Those three are one unit because an atom is a string in this cage and nothing else, so handing
/// them out separately would let a caller intern into one heap and read from another.
pub struct Isolate {
    heap: BumpHeap,
    atoms: AtomTable,
}

impl fmt::Debug for Isolate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Isolate")
            .field("heap_used", &self.heap.cursor())
            .field("atoms", &self.atoms.len())
            .finish()
    }
}

impl Isolate {
    /// Create an isolate with an empty heap.
    ///
    /// # Errors
    ///
    /// Returns [`CageError`] if the four gigabyte cage cannot be reserved, which on a modern
    /// sixty four bit system means the process is out of address space rather than out of memory.
    pub fn new() -> Result<Isolate, CageError> {
        Ok(Isolate {
            heap: BumpHeap::new()?,
            atoms: AtomTable::new(),
        })
    }

    /// The heap, for reading.
    #[must_use]
    pub const fn heap(&self) -> &BumpHeap {
        &self.heap
    }

    /// The heap, for allocating into.
    pub const fn heap_mut(&mut self) -> &mut BumpHeap {
        &mut self.heap
    }

    /// The cage every reference in this isolate is an offset into.
    #[must_use]
    pub const fn cage(&self) -> &Cage {
        self.heap.cage()
    }

    /// The atom table.
    #[must_use]
    pub const fn atoms(&self) -> &AtomTable {
        &self.atoms
    }

    /// Allocate a string, without interning it.
    ///
    /// Returns `None` if the heap is full. Used for strings a program computes, which are usually
    /// used once and are not worth the hash an atom costs.
    pub fn allocate_string(&mut self, text: &str) -> Option<StringRef> {
        StringRef::from_str(&mut self.heap, text)
    }

    /// Intern a string, so that every mention of the same text reaches one object.
    ///
    /// Returns `None` if the heap is full. Used for the things a program mentions by name over
    /// and over: identifiers, property keys and string literals.
    pub fn intern(&mut self, text: &str) -> Option<Atom> {
        self.atoms.intern(&mut self.heap, text)
    }

    /// Bytes this isolate has allocated on its heap.
    ///
    /// The number the memory budget in `spec/02-the-10x-goal.md` counts, which is what has been
    /// used rather than the four gigabytes of address space the cage reserves. Reserving is free
    /// and using is not.
    #[must_use]
    pub const fn heap_used(&self) -> usize {
        self.heap.cursor()
    }
}

#[cfg(test)]
mod tests {
    use super::Isolate;

    #[test]
    fn an_isolate_can_be_moved_to_another_thread() {
        let isolate = Isolate::new().expect("should reserve a cage");
        let used = std::thread::spawn(move || isolate.heap_used())
            .join()
            .expect("thread should not panic");
        assert_eq!(
            used, 0,
            "an isolate that has run nothing has allocated nothing"
        );
    }

    #[test]
    fn two_isolates_have_two_heaps() {
        // The property that makes an isolate the unit of parallelism: a string allocated in one is
        // not reachable from the other, and neither of them can see the other's cursor move.
        let mut first = Isolate::new().expect("should reserve a cage");
        let second = Isolate::new().expect("should reserve a cage");
        first
            .allocate_string("something")
            .expect("should have room");
        assert!(first.heap_used() > 0);
        assert_eq!(second.heap_used(), 0);
    }

    #[test]
    fn interning_the_same_text_twice_reaches_the_same_object() {
        let mut isolate = Isolate::new().expect("should reserve a cage");
        let first = isolate.intern("length").expect("should have room");
        let after_first = isolate.heap_used();
        let second = isolate.intern("length").expect("should have room");
        assert_eq!(first, second);
        assert_eq!(
            isolate.heap_used(),
            after_first,
            "the second intern should not allocate"
        );
    }
}
