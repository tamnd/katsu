//! The interpreter, the object model, shapes, inline caches and isolates.
//!
//! This is the largest crate in the workspace and the one everything above it depends on.
//! See `spec/05-interpreter.md` for dispatch, `spec/07-object-model.md` for values and
//! shapes, and `spec/03-architecture.md` for why an isolate is `Send` but not `Sync`.

mod global;
mod interpret;
mod native;
mod number;
mod output;
mod stack;
mod unit;
mod value;

use std::fmt;

use katsu_gc::{Atom, AtomTable, BumpHeap, Cage, CageError, StringRef};
use katsu_ir::FunctionBlueprint;

pub use global::Globals;
pub use interpret::{Interpreter, Interrupt, RuntimeError};
pub use native::{NativeFn, Natives, arg};
pub use output::{Discard, Output, Recorder, Standard, Stream};
pub use stack::{Frame, Invocation, Stack, StackError};
pub use unit::{Loaded, Resolved, Unit};
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
///
/// The globals and the table of functions written in Rust are here for the same reason and for one
/// more. A global is bound to a name that is a string in this cage, and a native is an ordinal that
/// means nothing except against this table, so both of them are as tied to this heap as an atom is.
/// The other reason is size: the dispatch loop holds the isolate behind a pointer precisely so that
/// what it holds inline stays small, and hanging a hash map and a vector off the isolate rather than
/// off the interpreter keeps the thing the loop touches every instruction exactly the size it was.
///
/// Strictly these two are a realm rather than an isolate, and one isolate can hold several realms
/// once there is a way to make one. There is not, so a second type today would be a type with one
/// instance.
///
/// The output sink is here on the same grounds. It is per realm rather than per process, because two
/// isolates on two threads printing into one buffer is exactly the thing this design is built to
/// avoid, and because an embedder that runs a script wants that script's output rather than every
/// script's output.
pub struct Isolate {
    heap: BumpHeap,
    atoms: AtomTable,
    globals: Globals,
    natives: Natives,
    output: Box<dyn Output>,
}

impl fmt::Debug for Isolate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Isolate")
            .field("heap_used", &self.heap.cursor())
            .field("atoms", &self.atoms.len())
            .field("globals", &self.globals.len())
            .field("natives", &self.natives)
            .field("output", &self.output)
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
            globals: Globals::new(),
            natives: Natives::new(),
            output: Box::new(Standard),
        })
    }

    /// Send everything this isolate prints somewhere other than the process's own streams.
    ///
    /// Returns the sink that was there, which is [`Standard`] on an isolate nobody has changed.
    /// Returning it rather than dropping it means a caller can put a recorder in, read what a
    /// script printed and put the old sink back, which is what a test harness does.
    pub fn set_output(&mut self, output: Box<dyn Output>) -> Box<dyn Output> {
        std::mem::replace(&mut self.output, output)
    }

    /// Write to this isolate's sink.
    ///
    /// The text goes out exactly as given. Deciding that a line ends with a newline is the builtin's
    /// job, because `console.log` adds one and `process.stdout.write` does not.
    pub fn write_output(&mut self, stream: Stream, text: &str) {
        self.output.write(stream, text);
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

    /// The global bindings, for reading.
    #[must_use]
    pub const fn globals(&self) -> &Globals {
        &self.globals
    }

    /// The global bindings, for binding something.
    pub const fn globals_mut(&mut self) -> &mut Globals {
        &mut self.globals
    }

    /// The table of functions written in Rust.
    #[must_use]
    pub const fn natives(&self) -> &Natives {
        &self.natives
    }

    /// The table of functions written in Rust, for adding one.
    pub const fn natives_mut(&mut self) -> &mut Natives {
        &mut self.natives
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
