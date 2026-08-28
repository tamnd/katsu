//! The instruction set.
//!
//! Register based and three address, in the Lua and Ignition lineage that spec 5.1 argues for. Every
//! operand names where its input lives instead of implying it, which costs a byte or two per
//! instruction and pays for itself in tier 2, where a stack machine would have to reconstruct the
//! value stack before it could build SSA.
//!
//! This is the decoded form, one Rust enum with one variant per opcode. Spec 5.1 also specifies a
//! byte encoding, one byte of opcode followed by operands with a wide prefix for functions past 256
//! registers, and that encoding is not here yet because nothing serializes bytecode until the on disk
//! cache lands. The decoded form is what the interpreter matches on, so the two will exist side by
//! side and the encoding is a lowering of this rather than a replacement for it.
//!
//! Jumps carry an absolute instruction index rather than a signed offset. In the encoded form an
//! offset is smaller and relocatable, and here an absolute index is the one that cannot be wrong by
//! a sign, which matters more while the patch list in lowering is new.
//!
//! Every opcode in this file has a construct in the M0 subset that lowers to it. The families in the
//! spec that M0 has no syntax for, iteration, generators, modules, private names and exception
//! handlers, are absent on purpose: an opcode with no producer is an opcode whose semantics nobody
//! has had to think about, and it would sit here looking implemented.
//!
//! Every opcode that can be polymorphic carries an inline cache slot index, which in JavaScript
//! means all property access, all calls, all arithmetic and all comparison, because `+` dispatches
//! on its operand types just as much as `o.x` dispatches on a shape. `Not` and `TypeOf` do not carry
//! one: both are total functions of the value's tag, so there is nothing for a cache to learn.

use std::fmt;

use crate::constant::ConstIndex;

/// A virtual register index within a frame.
///
/// The bytecode is register based rather than stack based, so that the operand of an instruction
/// names where its input lives instead of implying it. This is what makes the copy and patch
/// stencils in `spec/06-jit-tiers.md` tractable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Register(pub u16);

impl fmt::Debug for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// An index into a blueprint's inline cache slab.
///
/// Caches are allocated per site at lowering time rather than lazily, so that tier 1 can address
/// them with a constant offset. See `spec/07-object-model.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CacheIndex(pub u32);

impl fmt::Display for CacheIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ic{}", self.0)
    }
}

/// An instruction index within one blueprint's code, used as a jump target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CodeOffset(pub u32);

impl fmt::Display for CodeOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

/// An index into the enclosing blueprint's list of nested blueprints.
///
/// Nested rather than a flat per module table, because a blueprint plus everything reachable from it
/// is then one self contained thing to hand to a realm, to cache on disk, or to hand to the ahead of
/// time emitter, with no separate table to keep in step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlueprintIndex(pub u32);

impl fmt::Display for BlueprintIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn{}", self.0)
    }
}

/// A three address bytecode instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `dst = constants[src]`
    LoadConst { dst: Register, src: ConstIndex },
    /// `dst = value`, for an integer valued literal small enough to be an immediate.
    ///
    /// Saves a pool entry and a load for the numbers real code is full of, which are loop counters
    /// and array indices and the constant one.
    LoadInt { dst: Register, value: i32 },
    /// `dst = undefined`
    LoadUndefined { dst: Register },
    /// `dst = null`
    LoadNull { dst: Register },
    /// `dst = value`
    LoadBool { dst: Register, value: bool },
    /// `dst = this`, read from the fixed slot in the frame prologue in spec 5.4.
    LoadThis { dst: Register },
    /// `dst = the closure that is running`, which is what a named function expression sees itself as.
    ///
    /// `const f = function me() { return me; }` binds `me` inside the function and nowhere else, and
    /// the value it binds is the closure being called rather than whatever `f` holds now. There is
    /// nothing else to read it from, so it is an opcode.
    LoadClosure { dst: Register },
    /// `dst = src`
    Move { dst: Register, src: Register },

    /// Write the hole into `dst`, the value that makes a dead zone check fire.
    ///
    /// Written into a `let` or `const` slot when its scope is entered, so that a read before the
    /// declaration runs finds something that is not a value. Only emitted for bindings that some
    /// reference actually checks, so the ordinary `let x = 1;` costs nothing.
    LoadUninitialized { dst: Register },
    /// Throw a ReferenceError naming `constants[name]` if `src` holds the hole.
    ThrowIfUninitialized { src: Register, name: ConstIndex },
    /// Throw the TypeError that assigning to a `const` binding produces.
    ///
    /// A separate opcode rather than a general throw because the message has no operands in it, and
    /// because Node reports this at run time rather than as an early error, so lowering has to emit
    /// something that runs.
    ThrowConstAssignment,

    /// Read cell number `slot` from the environment `hops` links up the chain into `dst`.
    LoadUpvalue { dst: Register, hops: u16, slot: u16 },
    /// Write `src` into cell number `slot` in the environment `hops` links up the chain.
    StoreUpvalue { hops: u16, slot: u16, src: Register },
    /// Allocate this frame's environment with room for `size` cells and link it to the enclosing one.
    ///
    /// Emitted only for a function that has at least one captured binding, which is what makes hops
    /// count environments rather than function boundaries.
    NewContext { size: u16 },

    /// Read the global named `constants[name]` into `dst`, throwing if there is no such binding.
    LoadGlobal {
        dst: Register,
        name: ConstIndex,
        cache: CacheIndex,
    },
    /// Read the global named `constants[name]` into `dst`, or undefined if there is no such binding.
    ///
    /// The one place in the language where reading an undeclared name is not an error, which is why
    /// it cannot be a load followed by a `TypeOf` and has to be its own opcode.
    LoadGlobalForTypeof {
        dst: Register,
        name: ConstIndex,
        cache: CacheIndex,
    },
    /// Write `src` into the global named `constants[name]`.
    StoreGlobal {
        name: ConstIndex,
        src: Register,
        cache: CacheIndex,
    },

    /// `dst = lhs + rhs`, with the full ToPrimitive dance behind the cache.
    Add {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs - rhs`
    Sub {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs * rhs`
    Mul {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs / rhs`
    Div {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs % rhs`
    Rem {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs ** rhs`
    Pow {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs | rhs`
    BitOr {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs ^ rhs`
    BitXor {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs & rhs`
    BitAnd {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs << rhs`
    Shl {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs >> rhs`, the sign propagating shift.
    Shr {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs >>> rhs`, the zero filling shift.
    UnsignedShr {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },

    /// `dst = lhs == rhs`
    Equal {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs != rhs`
    NotEqual {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs === rhs`
    StrictEqual {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs !== rhs`
    StrictNotEqual {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs < rhs`
    Less {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs <= rhs`
    LessEqual {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs > rhs`
    Greater {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs >= rhs`
    GreaterEqual {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs in rhs`
    In {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },
    /// `dst = lhs instanceof rhs`
    InstanceOf {
        dst: Register,
        lhs: Register,
        rhs: Register,
        cache: CacheIndex,
    },

    /// `dst = -src`
    Neg {
        dst: Register,
        src: Register,
        cache: CacheIndex,
    },
    /// `dst = ~src`
    BitNot {
        dst: Register,
        src: Register,
        cache: CacheIndex,
    },
    /// `dst = +src`, which is ToNumber and not a no operation.
    ToNumber {
        dst: Register,
        src: Register,
        cache: CacheIndex,
    },
    /// `dst = ToNumeric(src) + 1`, the increment half of `++`.
    Inc {
        dst: Register,
        src: Register,
        cache: CacheIndex,
    },
    /// `dst = ToNumeric(src) - 1`, the decrement half of `--`.
    Dec {
        dst: Register,
        src: Register,
        cache: CacheIndex,
    },
    /// `dst = !src`
    Not { dst: Register, src: Register },
    /// `dst = typeof src`
    TypeOf { dst: Register, src: Register },

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
    /// `dst = obj[index]`, with the key computed at run time.
    GetIndex {
        dst: Register,
        obj: Register,
        index: Register,
        cache: CacheIndex,
    },
    /// `obj[index] = value`
    SetIndex {
        obj: Register,
        index: Register,
        value: Register,
        cache: CacheIndex,
    },
    /// `dst = delete obj[constants[key]]`
    DeleteProp {
        dst: Register,
        obj: Register,
        key: ConstIndex,
    },
    /// `dst = delete obj[index]`
    DeleteIndex {
        dst: Register,
        obj: Register,
        index: Register,
    },

    /// `dst = callee(args...)`, with the arguments in consecutive registers from `args`.
    ///
    /// Consecutive registers rather than a list, because the callee's frame is built by copying a
    /// contiguous run, and because it is the shape the argument adaptor needs when the count and the
    /// arity disagree. It is a real constraint on the register allocator and it is written down in
    /// spec 4.5 as one.
    Call {
        dst: Register,
        callee: Register,
        args: Register,
        argc: u16,
        cache: CacheIndex,
    },
    /// `dst = obj[constants[key]](args...)`, with `obj` as the receiver.
    ///
    /// Separate from `Call` so that a method call does not have to materialise the function into a
    /// register and then lose track of which object it came from. Preserving the receiver is the
    /// whole difference and it is the most common call shape in real code.
    CallMethod {
        dst: Register,
        obj: Register,
        key: ConstIndex,
        args: Register,
        argc: u16,
        cache: CacheIndex,
    },
    /// Return `src` to the caller.
    Return { src: Register },

    /// Throw `src`.
    ///
    /// Where control goes is not in the instruction, and that is the design rather than an
    /// omission. Every function carries a table of the ranges it protects, so entering a `try`
    /// costs nothing at all and a throw pays for the search. `spec/04-frontend.md` picked that
    /// trade the way the JVM picked it, because a `try` inside a hot loop is far more common than
    /// a throw inside one.
    Throw { src: Register },

    /// Stamp a closure out of `blueprint`, capturing the current environment, into `dst`.
    NewClosure {
        dst: Register,
        blueprint: BlueprintIndex,
    },

    /// Put a new empty object in `dst`, with room inside it for `slots` properties.
    ///
    /// The properties are not here. An object literal is this instruction followed by one
    /// `set_prop` per property, which is three or four instructions where one would do and is
    /// deliberate: a store is the operation that takes the shape transition, so building a literal
    /// out of stores means a literal and an object grown a property at a time reach the same shape,
    /// and neither of them needs a second code path. It also means a literal is already inline
    /// cacheable at each store without a new kind of cache.
    ///
    /// `slots` is a promise about what is about to be stored rather than a limit. It is what makes
    /// the literal one allocation instead of one plus a properties array, and an object that ends up
    /// with more than it was built for grows the ordinary way.
    NewObject { dst: Register, slots: u16 },

    /// Continue at `target`.
    Jump { target: CodeOffset },
    /// Continue at `target` if `cond` is truthy.
    JumpIfTrue { cond: Register, target: CodeOffset },
    /// Continue at `target` if `cond` is falsy.
    JumpIfFalse { cond: Register, target: CodeOffset },
    /// Continue at `target`, counting the iteration and checking the interrupt flag.
    ///
    /// The counter is what triggers tier up and on stack replacement, and the interrupt check is the
    /// one mechanism from spec 5.6 that covers collection safepoints, timeouts and worker
    /// termination. Both hang off a back edge, so a back edge is its own opcode rather than a jump
    /// that happens to point upwards.
    LoopBackEdge {
        target: CodeOffset,
        profile: CacheIndex,
    },
}

impl Op {
    /// The constant pool entry this instruction reads, if it reads one.
    ///
    /// Used by a disassembly to print the name rather than the index, and by anything that has to
    /// know which pool entries a stretch of code depends on.
    pub const fn constant(self) -> Option<ConstIndex> {
        match self {
            Self::LoadConst { src, .. } => Some(src),
            Self::ThrowIfUninitialized { name, .. }
            | Self::LoadGlobal { name, .. }
            | Self::LoadGlobalForTypeof { name, .. }
            | Self::StoreGlobal { name, .. } => Some(name),
            Self::GetProp { key, .. }
            | Self::SetProp { key, .. }
            | Self::DeleteProp { key, .. }
            | Self::CallMethod { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Where this instruction can transfer control to, if it can.
    ///
    /// A verifier walks these to check that every target is in range, and the patch list in lowering
    /// is checked against them so that a forward jump left unpatched is a test failure rather than a
    /// jump to instruction zero.
    pub const fn jump_target(self) -> Option<CodeOffset> {
        match self {
            Self::Jump { target }
            | Self::JumpIfTrue { target, .. }
            | Self::JumpIfFalse { target, .. }
            | Self::LoopBackEdge { target, .. } => Some(target),
            _ => None,
        }
    }

    /// Rewrite this instruction's jump target, which is how a forward reference is patched.
    pub fn set_jump_target(&mut self, to: CodeOffset) {
        match self {
            Self::Jump { target }
            | Self::JumpIfTrue { target, .. }
            | Self::JumpIfFalse { target, .. }
            | Self::LoopBackEdge { target, .. } => *target = to,
            _ => panic!("{self:?} is not a jump"),
        }
    }

    /// Whether control never continues to the next instruction.
    ///
    /// Lowering uses it to avoid emitting the implicit `return undefined` after a body that already
    /// ended in a return, which is the difference between a tidy disassembly and one with dead tails
    /// all through it. `verify` uses it to check that the last instruction is one, because control
    /// running off the end of the code is the one bytecode bug the interpreter cannot report.
    ///
    /// A back edge is on the list because it is an unconditional jump with a counter attached, so
    /// nothing after it ever runs. That matters for `while (true) {}`, whose whole body is a loop
    /// with no exit and whose last instruction is therefore the edge itself.
    pub const fn is_terminator(self) -> bool {
        matches!(
            self,
            Self::Return { .. }
                | Self::Jump { .. }
                | Self::LoopBackEdge { .. }
                | Self::Throw { .. }
                | Self::ThrowConstAssignment
        )
    }
}

impl fmt::Display for Op {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LoadConst { dst, src } => write!(f, "load_const {dst}, k{}", src.0),
            Self::LoadInt { dst, value } => write!(f, "load_int {dst}, {value}"),
            Self::LoadUndefined { dst } => write!(f, "load_undefined {dst}"),
            Self::LoadNull { dst } => write!(f, "load_null {dst}"),
            Self::LoadBool { dst, value } => write!(f, "load_bool {dst}, {value}"),
            Self::LoadThis { dst } => write!(f, "load_this {dst}"),
            Self::LoadClosure { dst } => write!(f, "load_closure {dst}"),
            Self::Move { dst, src } => write!(f, "move {dst}, {src}"),
            Self::LoadUninitialized { dst } => write!(f, "load_uninitialized {dst}"),
            Self::ThrowIfUninitialized { src, name } => {
                write!(f, "throw_if_uninitialized {src}, k{}", name.0)
            }
            Self::ThrowConstAssignment => write!(f, "throw_const_assignment"),
            Self::LoadUpvalue { dst, hops, slot } => {
                write!(f, "load_upvalue {dst}, {hops}, {slot}")
            }
            Self::StoreUpvalue { hops, slot, src } => {
                write!(f, "store_upvalue {hops}, {slot}, {src}")
            }
            Self::NewContext { size } => write!(f, "new_context {size}"),
            Self::LoadGlobal { dst, name, cache } => {
                write!(f, "load_global {dst}, k{}, {cache}", name.0)
            }
            Self::LoadGlobalForTypeof { dst, name, cache } => {
                write!(f, "load_global_for_typeof {dst}, k{}, {cache}", name.0)
            }
            Self::StoreGlobal { name, src, cache } => {
                write!(f, "store_global k{}, {src}, {cache}", name.0)
            }
            Self::Add { .. }
            | Self::Sub { .. }
            | Self::Mul { .. }
            | Self::Div { .. }
            | Self::Rem { .. }
            | Self::Pow { .. }
            | Self::BitOr { .. }
            | Self::BitXor { .. }
            | Self::BitAnd { .. }
            | Self::Shl { .. }
            | Self::Shr { .. }
            | Self::UnsignedShr { .. }
            | Self::Equal { .. }
            | Self::NotEqual { .. }
            | Self::StrictEqual { .. }
            | Self::StrictNotEqual { .. }
            | Self::Less { .. }
            | Self::LessEqual { .. }
            | Self::Greater { .. }
            | Self::GreaterEqual { .. }
            | Self::In { .. }
            | Self::InstanceOf { .. } => self.write_three_address(f),
            Self::Neg { dst, src, cache } => write!(f, "neg {dst}, {src}, {cache}"),
            Self::BitNot { dst, src, cache } => write!(f, "bit_not {dst}, {src}, {cache}"),
            Self::ToNumber { dst, src, cache } => write!(f, "to_number {dst}, {src}, {cache}"),
            Self::Inc { dst, src, cache } => write!(f, "inc {dst}, {src}, {cache}"),
            Self::Dec { dst, src, cache } => write!(f, "dec {dst}, {src}, {cache}"),
            Self::Not { dst, src } => write!(f, "not {dst}, {src}"),
            Self::TypeOf { dst, src } => write!(f, "type_of {dst}, {src}"),
            Self::GetProp {
                dst,
                obj,
                key,
                cache,
            } => write!(f, "get_prop {dst}, {obj}, k{}, {cache}", key.0),
            Self::SetProp {
                obj,
                key,
                value,
                cache,
            } => write!(f, "set_prop {obj}, k{}, {value}, {cache}", key.0),
            Self::GetIndex {
                dst,
                obj,
                index,
                cache,
            } => write!(f, "get_index {dst}, {obj}, {index}, {cache}"),
            Self::SetIndex {
                obj,
                index,
                value,
                cache,
            } => write!(f, "set_index {obj}, {index}, {value}, {cache}"),
            Self::DeleteProp { dst, obj, key } => {
                write!(f, "delete_prop {dst}, {obj}, k{}", key.0)
            }
            Self::DeleteIndex { dst, obj, index } => {
                write!(f, "delete_index {dst}, {obj}, {index}")
            }
            Self::Call {
                dst,
                callee,
                args,
                argc,
                cache,
            } => write!(f, "call {dst}, {callee}, {args}, {argc}, {cache}"),
            Self::CallMethod {
                dst,
                obj,
                key,
                args,
                argc,
                cache,
            } => write!(
                f,
                "call_method {dst}, {obj}, k{}, {args}, {argc}, {cache}",
                key.0
            ),
            Self::Return { src } => write!(f, "return {src}"),
            Self::Throw { src } => write!(f, "throw {src}"),
            Self::NewClosure { dst, blueprint } => write!(f, "new_closure {dst}, {blueprint}"),
            Self::NewObject { dst, slots } => write!(f, "new_object {dst}, {slots}"),
            Self::Jump { target } => write!(f, "jump {target}"),
            Self::JumpIfTrue { cond, target } => write!(f, "jump_if_true {cond}, {target}"),
            Self::JumpIfFalse { cond, target } => write!(f, "jump_if_false {cond}, {target}"),
            Self::LoopBackEdge { target, profile } => {
                write!(f, "loop_back_edge {target}, {profile}")
            }
        }
    }
}

impl Op {
    /// Print the two operand and one destination shape that most of the set has.
    ///
    /// Written once rather than twenty two times, because these arms differ only in a mnemonic and
    /// a wall of near identical `write!` calls is where a copied and half edited line hides. It is a
    /// long function because twenty two operators is a long list, and a list is not complexity.
    #[allow(clippy::too_many_lines)]
    fn write_three_address(self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, dst, lhs, rhs, cache) = match self {
            Self::Add {
                dst,
                lhs,
                rhs,
                cache,
            } => ("add", dst, lhs, rhs, cache),
            Self::Sub {
                dst,
                lhs,
                rhs,
                cache,
            } => ("sub", dst, lhs, rhs, cache),
            Self::Mul {
                dst,
                lhs,
                rhs,
                cache,
            } => ("mul", dst, lhs, rhs, cache),
            Self::Div {
                dst,
                lhs,
                rhs,
                cache,
            } => ("div", dst, lhs, rhs, cache),
            Self::Rem {
                dst,
                lhs,
                rhs,
                cache,
            } => ("rem", dst, lhs, rhs, cache),
            Self::Pow {
                dst,
                lhs,
                rhs,
                cache,
            } => ("pow", dst, lhs, rhs, cache),
            Self::BitOr {
                dst,
                lhs,
                rhs,
                cache,
            } => ("bit_or", dst, lhs, rhs, cache),
            Self::BitXor {
                dst,
                lhs,
                rhs,
                cache,
            } => ("bit_xor", dst, lhs, rhs, cache),
            Self::BitAnd {
                dst,
                lhs,
                rhs,
                cache,
            } => ("bit_and", dst, lhs, rhs, cache),
            Self::Shl {
                dst,
                lhs,
                rhs,
                cache,
            } => ("shl", dst, lhs, rhs, cache),
            Self::Shr {
                dst,
                lhs,
                rhs,
                cache,
            } => ("shr", dst, lhs, rhs, cache),
            Self::UnsignedShr {
                dst,
                lhs,
                rhs,
                cache,
            } => ("unsigned_shr", dst, lhs, rhs, cache),
            Self::Equal {
                dst,
                lhs,
                rhs,
                cache,
            } => ("equal", dst, lhs, rhs, cache),
            Self::NotEqual {
                dst,
                lhs,
                rhs,
                cache,
            } => ("not_equal", dst, lhs, rhs, cache),
            Self::StrictEqual {
                dst,
                lhs,
                rhs,
                cache,
            } => ("strict_equal", dst, lhs, rhs, cache),
            Self::StrictNotEqual {
                dst,
                lhs,
                rhs,
                cache,
            } => ("strict_not_equal", dst, lhs, rhs, cache),
            Self::Less {
                dst,
                lhs,
                rhs,
                cache,
            } => ("less", dst, lhs, rhs, cache),
            Self::LessEqual {
                dst,
                lhs,
                rhs,
                cache,
            } => ("less_equal", dst, lhs, rhs, cache),
            Self::Greater {
                dst,
                lhs,
                rhs,
                cache,
            } => ("greater", dst, lhs, rhs, cache),
            Self::GreaterEqual {
                dst,
                lhs,
                rhs,
                cache,
            } => ("greater_equal", dst, lhs, rhs, cache),
            Self::In {
                dst,
                lhs,
                rhs,
                cache,
            } => ("in", dst, lhs, rhs, cache),
            Self::InstanceOf {
                dst,
                lhs,
                rhs,
                cache,
            } => ("instance_of", dst, lhs, rhs, cache),
            other => unreachable!("{other:?} is not a three address instruction"),
        };
        write!(f, "{name} {dst}, {lhs}, {rhs}, {cache}")
    }
}

#[cfg(test)]
mod tests {
    use super::{BlueprintIndex, CacheIndex, CodeOffset, Op, Register};
    use crate::constant::ConstIndex;

    #[test]
    fn registers_print_the_way_a_disassembly_should_read() {
        assert_eq!(format!("{:?}", Register(7)), "r7");
        assert_eq!(Register(7).to_string(), "r7");
    }

    #[test]
    fn an_instruction_prints_its_mnemonic_and_its_operands() {
        let add = Op::Add {
            dst: Register(0),
            lhs: Register(1),
            rhs: Register(2),
            cache: CacheIndex(3),
        };
        assert_eq!(add.to_string(), "add r0, r1, r2, ic3");
    }

    #[test]
    fn the_running_closure_is_its_own_instruction() {
        // `const f = function me() { return me; };` binds `me` to the closure being called and not
        // to whatever `f` holds now, and there is nothing else to read that from.
        let load = Op::LoadClosure { dst: Register(0) };
        assert_eq!(load.to_string(), "load_closure r0");
        assert!(!load.is_terminator());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn the_shared_three_address_printer_covers_every_operator_it_claims_to() {
        // Every arm routed through `write_three_address` has to come back out with a mnemonic. If
        // one is added to the enum and forgotten there, this panics on the `unreachable`.
        let operands = (Register(0), Register(1), Register(2), CacheIndex(0));
        let (dst, lhs, rhs, cache) = operands;
        let ops = [
            Op::Add {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Sub {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Mul {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Div {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Rem {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Pow {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::BitOr {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::BitXor {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::BitAnd {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Shl {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Shr {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::UnsignedShr {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Equal {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::NotEqual {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::StrictEqual {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::StrictNotEqual {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Less {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::LessEqual {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::Greater {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::GreaterEqual {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::In {
                dst,
                lhs,
                rhs,
                cache,
            },
            Op::InstanceOf {
                dst,
                lhs,
                rhs,
                cache,
            },
        ];

        for op in ops {
            let printed = op.to_string();
            assert!(
                printed.ends_with("r0, r1, r2, ic0"),
                "{op:?} printed as {printed}"
            );
            assert!(!printed.starts_with(' '), "{op:?} printed with no mnemonic");
        }
    }

    #[test]
    fn only_the_instructions_that_read_a_constant_report_one() {
        assert_eq!(
            Op::GetProp {
                dst: Register(0),
                obj: Register(1),
                key: ConstIndex(4),
                cache: CacheIndex(0),
            }
            .constant(),
            Some(ConstIndex(4))
        );
        assert_eq!(Op::LoadUndefined { dst: Register(0) }.constant(), None);
    }

    #[test]
    fn a_forward_jump_can_be_patched_once_the_target_is_known() {
        let mut jump = Op::JumpIfFalse {
            cond: Register(0),
            target: CodeOffset(u32::MAX),
        };
        jump.set_jump_target(CodeOffset(12));

        assert_eq!(jump.jump_target(), Some(CodeOffset(12)));
        assert_eq!(jump.to_string(), "jump_if_false r0, @12");
    }

    #[test]
    #[should_panic(expected = "is not a jump")]
    fn patching_something_that_is_not_a_jump_is_a_bug_and_says_so() {
        Op::Return { src: Register(0) }.set_jump_target(CodeOffset(0));
    }

    #[test]
    fn a_return_ends_a_run_of_instructions_and_a_load_does_not() {
        assert!(Op::Return { src: Register(0) }.is_terminator());
        assert!(
            Op::Jump {
                target: CodeOffset(0)
            }
            .is_terminator()
        );
        // An unconditional jump with a counter attached, so nothing after it ever runs.
        assert!(
            Op::LoopBackEdge {
                target: CodeOffset(0),
                profile: CacheIndex(0)
            }
            .is_terminator()
        );
        // And the conditional ones are not, because control does continue when the test fails.
        assert!(
            !Op::JumpIfTrue {
                cond: Register(0),
                target: CodeOffset(0)
            }
            .is_terminator()
        );
        assert!(
            !Op::Move {
                dst: Register(0),
                src: Register(1)
            }
            .is_terminator()
        );
    }

    #[test]
    fn a_closure_names_the_blueprint_it_is_stamped_from() {
        let op = Op::NewClosure {
            dst: Register(2),
            blueprint: BlueprintIndex(1),
        };
        assert_eq!(op.to_string(), "new_closure r2, fn1");
    }
}
