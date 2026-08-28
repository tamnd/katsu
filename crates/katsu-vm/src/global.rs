//! The global bindings, which is where a name goes when it belongs to nobody in particular.
//!
//! Everything a program declares at the top of a function is a register, and everything a nested
//! function reads is a context cell, and both of those are decided at compile time by scope
//! analysis. What is left is every name that analysis could not place: `console`, `Math`,
//! `globalThis`, a function declared at the top level of a script, and an assignment to a name that
//! was never declared anywhere. Those all end up here.
//!
//! # Why this is a map and not an object
//!
//! In the language, the global scope is an ordinary object with ordinary properties, which is why
//! `globalThis.x = 1` and `x = 1` do the same thing and why deleting a global is spelled `delete`.
//! Implementing it that way needs shapes, property storage and inline caches, and all three of those
//! are M1. So this is a map from an interned name to a value, which is the same behaviour minus the
//! parts of it no program in M0 can reach: there is no `delete`, no property descriptor, no
//! prototype above the global object and no way to enumerate it. When the object model lands, this
//! type goes away and the global object becomes an object like any other, which is the point at
//! which the `cache` operand on the three global opcodes starts holding a cell pointer instead of
//! being ignored.
//!
//! # Why the key is a slot and not a string
//!
//! Every name in a constant pool is interned when the unit is loaded, and interning means one
//! address per distinct text in this cage. So two mentions of `console` anywhere in a program are
//! the same address, and comparing names is comparing four bytes rather than walking two strings.
//! The key here is those four bytes.
//!
//! What that costs is a rule the callers have to keep: a name has to be interned before it is looked
//! up, because a string with the same text at a different address will simply miss. Every caller
//! today satisfies it by construction, since the only two ways in are a constant pool that was
//! interned on load and [`crate::Interpreter::define_global`], which interns. The way a program
//! could produce a name that was not interned is `globalThis[computed]`, and indexing an object is
//! not implemented yet. When it is, the lookup goes through the object model rather than through
//! here, so the rule expires rather than growing a check.
//!
//! # What the collector will want
//!
//! Both halves of every entry are roots: the value obviously, and the key because it is the only
//! reference to the name string that a global holds. A collector walks this table alongside
//! [`crate::Stack::roots`] and the frame contexts, and moving a name would mean rekeying rather than
//! rewriting a field, which is one more reason M1 replaces this with a real object.

use rustc_hash::FxHashMap;

use katsu_gc::StringRef;

use crate::Value;

/// The bindings a program reaches by name when nothing lexical claimed the name first.
#[derive(Debug, Default)]
pub struct Globals {
    /// Keyed by the raw bits of the name's slot, which is its identity once it is interned.
    bindings: FxHashMap<u32, Value>,
}

impl Globals {
    /// An empty set of bindings.
    #[must_use]
    pub fn new() -> Globals {
        Globals::default()
    }

    /// The value bound to `name`, or `None` if nothing is.
    ///
    /// `None` is a `ReferenceError` at the load in front of it and `undefined` at a `typeof`, which
    /// is the one place in the language where reading a name that does not exist is allowed. Both of
    /// those decisions belong to the opcode rather than to the table, so this reports the fact and
    /// nothing else.
    #[must_use]
    pub fn get(&self, name: StringRef) -> Option<Value> {
        self.bindings.get(&name.slot().to_bits()).copied()
    }

    /// Bind `name` to `value`, replacing whatever was there.
    ///
    /// One method for both the declaration and the assignment, because the global scope does not
    /// tell them apart: an assignment to an undeclared name in sloppy mode creates the binding, and
    /// that is not an accident in the language, it is what the specification says. Strict mode makes
    /// it a `ReferenceError`, and that check belongs at the opcode when there is a strict flag to
    /// check.
    pub fn set(&mut self, name: StringRef, value: Value) {
        self.bindings.insert(name.slot().to_bits(), value);
    }

    /// How many bindings there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether nothing is bound, which is true of a realm nobody has built yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Every name bound here, for a collector to trace and for a test to read.
    pub fn names(&self) -> impl Iterator<Item = StringRef> + '_ {
        self.bindings
            .keys()
            .filter_map(|bits| StringRef::from_slot(katsu_gc::Slot::from_bits(*bits)))
    }

    /// Every value bound here, which is the other half of what a collector traces.
    pub fn values(&self) -> impl Iterator<Item = Value> + '_ {
        self.bindings.values().copied()
    }
}
