//! Bytecode, function blueprints and the opcode description language.
//!
//! This crate is the bottom of the pipeline that everything else agrees on. The parser
//! lowers into it, the interpreter executes it, the stencil generator derives tier 1 from
//! it, and the ahead of time emitter reads it. See `spec/05-interpreter.md` for the
//! bytecode design and `spec/04-frontend.md` for how a program gets here.

use std::fmt;

/// The version of the bytecode format itself, independent of the crate version.
///
/// Every cache file and every snapshot carries it. A mismatch means the artifact is
/// discarded and regenerated, silently and correctly, never loaded optimistically.
/// See `spec/16-package-layout.md`.
pub const BYTECODE_FORMAT_VERSION: u32 = 1;

/// A virtual register index within a frame.
///
/// The bytecode is register based rather than stack based, so that the operand of an
/// instruction names where its input lives instead of implying it. This is what makes
/// the copy and patch stencils in `spec/06-jit-tiers.md` tractable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Register(pub u16);

impl fmt::Debug for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// An index into a blueprint's constant pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConstIndex(pub u32);

/// An index into a blueprint's inline cache slab.
///
/// Caches are allocated per site at lowering time rather than lazily, so that tier 1 can
/// address them with a constant offset. See `spec/07-object-model.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CacheIndex(pub u32);

/// A three address bytecode instruction.
///
/// This is a deliberately small starting set. The full opcode table is generated from the
/// description language in `spec/05-interpreter.md` rather than written out by hand, so
/// that the interpreter and the baseline JIT cannot drift apart. That generator is M2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `dst = constants[src]`
    LoadConst { dst: Register, src: ConstIndex },
    /// `dst = undefined`
    LoadUndefined { dst: Register },
    /// `dst = src`
    Move { dst: Register, src: Register },
    /// `dst = lhs + rhs`, with the full ToPrimitive dance behind the cache.
    Add {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = obj[constants[key]]`, the operation the whole architecture is judged on.
    GetProp {
        dst: Register,
        obj: Register,
        key: ConstIndex,
        cache: CacheIndex,
    },
    /// `obj[constants[key]] = value`
    SetProp {
        obj: Register,
        key: ConstIndex,
        value: Register,
        cache: CacheIndex,
    },
    /// `dst = callee(args...)`, with the arguments in consecutive registers from `args`.
    Call {
        dst: Register,
        callee: Register,
        args: Register,
        argc: u16,
        cache: CacheIndex,
    },
    /// Return `src` to the caller.
    Return { src: Register },
}

/// Everything the runtime knows about one function before it has ever been called.
///
/// Named a blueprint rather than a code object because it is the thing a closure is
/// stamped out of, and because several closures share one. See `spec/04-frontend.md`.
#[derive(Clone, Debug, Default)]
pub struct FunctionBlueprint {
    /// The source name, for stack traces. Empty for an anonymous function.
    pub name: String,
    /// How many virtual registers a frame for this function needs.
    pub frame_size: u16,
    /// Declared parameter count, before rest and default handling.
    pub arity: u16,
    /// The instruction stream.
    pub code: Vec<Op>,
    /// How many inline cache slots this function's sites need.
    pub cache_slots: u32,
}

impl FunctionBlueprint {
    /// The number of instructions in this blueprint.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether this blueprint has no instructions at all, which only happens before lowering.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{ConstIndex, FunctionBlueprint, Op, Register};

    #[test]
    fn a_blueprint_holds_the_instructions_it_was_given() {
        let mut blueprint = FunctionBlueprint::default();
        assert!(blueprint.is_empty());

        blueprint.code.push(Op::LoadConst {
            dst: Register(0),
            src: ConstIndex(0),
        });
        blueprint.code.push(Op::Return { src: Register(0) });

        assert_eq!(blueprint.len(), 2);
        assert!(!blueprint.is_empty());
    }

    #[test]
    fn registers_print_the_way_a_disassembly_should_read() {
        assert_eq!(format!("{:?}", Register(7)), "r7");
    }
}
