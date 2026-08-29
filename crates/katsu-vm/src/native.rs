//! Functions whose body is Rust, and the table the interpreter calls them through.
//!
//! Every runtime needs a bottom. `console.log` writes to a file descriptor, `Date.now` reads a
//! clock, and no amount of bytecode gets to either of those on its own, so somewhere the call has to
//! leave the dispatch loop and land in a Rust function. This is that boundary, and it is one
//! function pointer and one bounds checked index wide.
//!
//! # The calling convention
//!
//! A native takes the interpreter, the receiver it was called on, and a slice of arguments, and
//! returns a value or an error. That is the whole of it, and each of the four parts is the way it is
//! for a reason.
//!
//! It takes `&mut Interpreter` because everything a native does needs the isolate: allocating the
//! string it returns, reading the string it was passed, printing a value. Handing it something
//! narrower would mean deciding today which of those a native is allowed to do, and getting that
//! wrong costs a signature change on every native there is.
//!
//! It takes a slice rather than a pointer into the stack because the interpreter is mutable for the
//! length of the call, so the arguments are copied out of the caller's registers first. Eight of
//! them fit without allocating, which covers every builtin worth writing, and the ninth spills to
//! the heap for that one call.
//!
//! The receiver is an option and not a value, and the difference is the same one [`Frame::receiver`]
//! makes: `None` means the call site supplied nothing rather than that it supplied `undefined`. A
//! builtin is neither strict nor sloppy, so what a plain call to one means depends on the code that
//! made the call, which is not something the native can see. Almost every native ignores it, and the
//! ones that do not go through [`this_value`], which refuses by name rather than guessing.
//!
//! [`Frame::receiver`]: crate::stack::Frame::receiver
//!
//! The slice is exactly as long as the call site passed, which is not the same as what the native
//! declares. JavaScript pads a short call with `undefined` and drops a long one's extras, and a
//! native that cares has to do that itself, which is what [`arg`] is for. Padding here instead would
//! mean picking an arity for every native and copying zeroes for the ones that never look.
//!
//! Returning `Result` means a native throws the same way an opcode does. There is no `try` to catch
//! it in M0, so it reaches the embedder, and when there is one nothing about this changes.
//!
//! # Re-entrancy
//!
//! A native holds the whole interpreter, so nothing stops it calling back into JavaScript, and
//! `Array.prototype.map` is exactly that. Two things are missing before one can: a native does not
//! push a frame, so it is invisible to the depth limit and to a stack trace, and there is no public
//! way to call a value from Rust yet. Both arrive with the first builtin that needs them rather than
//! being built for a caller that does not exist.

use crate::Value;
use crate::interpret::{Interpreter, RuntimeError};

/// The signature every function written in Rust has.
///
/// A plain function pointer and not a boxed closure, because a native closing over state would be
/// state the collector cannot see and the AOT image cannot serialise. What a native needs to reach,
/// it reaches through the interpreter it was handed.
pub type NativeFn = fn(&mut Interpreter, Option<Value>, &[Value]) -> Result<Value, RuntimeError>;

/// The receiver a native was called on, for a native that cannot work without one.
///
/// `None` means a plain call, and what `this` is in one depends on whether the code that made the
/// call is strict, which a builtin has no way to ask. Node answers `globalThis` for the sloppy case
/// and there is no global object yet, so this refuses by name instead of picking one of the two
/// answers and being wrong about half the calls.
///
/// # Errors
///
/// Returns [`RuntimeError::Unsupported`] when nothing was supplied.
pub fn this_value(receiver: Option<Value>, name: &str) -> Result<Value, RuntimeError> {
    receiver.ok_or_else(|| {
        RuntimeError::Unsupported(format!(
            "{name} called on nothing needs globalThis, which is not implemented yet"
        ))
    })
}

/// One argument, padded with `undefined` the way a call in the language is.
///
/// Every native that reads an argument goes through this rather than indexing, because `print()`
/// with nothing in the brackets is a call a program is allowed to make and an index would panic on
/// it.
#[must_use]
pub fn arg(args: &[Value], index: usize) -> Value {
    args.get(index).copied().unwrap_or(Value::UNDEFINED)
}

/// One entry: the code, and the name that code answers to.
struct Native {
    name: Box<str>,
    call: NativeFn,
}

/// Every function written in Rust that this isolate can call.
///
/// Indexed by the ordinal stored in the eight byte native object in the cage. The indirection is
/// there so that the cage holds no code pointers, which is the argument in `katsu_gc::NativeRef`,
/// and the table is on this side of the wall so that a native can name a type the heap has never
/// heard of.
///
/// It only grows. Entries are added when a realm is built and nothing removes one, so an ordinal
/// that was ever valid stays valid, which is what lets the object in the cage hold a bare integer
/// with no generation counter next to it.
#[derive(Default)]
pub struct Natives {
    entries: Vec<Native>,
}

impl std::fmt::Debug for Natives {
    /// The names rather than the addresses, because a list of function pointers tells a reader
    /// nothing and a list of names says which builtins a realm has.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.entries.iter().map(|entry| &entry.name))
            .finish()
    }
}

impl Natives {
    /// An empty table.
    #[must_use]
    pub fn new() -> Natives {
        Natives::default()
    }

    /// Add a function and hand back the ordinal that reaches it.
    ///
    /// Returns `None` if there are already four billion of them, which is not a situation a real
    /// program reaches and is checked because the alternative is an ordinal that truncates and calls
    /// the wrong function.
    pub fn add(&mut self, name: &str, call: NativeFn) -> Option<u32> {
        let ordinal = u32::try_from(self.entries.len()).ok()?;
        self.entries.push(Native {
            name: name.into(),
            call,
        });
        Some(ordinal)
    }

    /// The function at `ordinal`, or `None` if there is no such entry.
    ///
    /// Copied out rather than borrowed, because the caller is about to hand the interpreter to it
    /// and cannot be holding a reference into the isolate while it does.
    #[must_use]
    pub fn get(&self, ordinal: u32) -> Option<NativeFn> {
        self.entries.get(ordinal as usize).map(|entry| entry.call)
    }

    /// The name at `ordinal`, which is what printing a native shows.
    #[must_use]
    pub fn name(&self, ordinal: u32) -> Option<&str> {
        self.entries.get(ordinal as usize).map(|entry| &*entry.name)
    }

    /// How many functions the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty, which it is until a realm is built.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
