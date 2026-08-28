//! Everything the runtime knows about one function before it has ever been called.

use std::fmt;

use smallvec::{SmallVec, smallvec};

use crate::constant::{ConstIndex, ConstantPool};
use crate::op::{BlueprintIndex, CacheIndex, CodeOffset, Op, Register};
use crate::position::SourcePositions;

/// A blueprint that does not hold together.
///
/// Only ever produced by `verify`, and only ever the result of a bug in whatever built the
/// blueprint. Every variant names the instruction it was found at, because the first question is
/// always which one.
#[derive(Debug, thiserror::Error)]
pub enum BlueprintError {
    /// A jump points outside the code.
    #[error("instruction {at} jumps to {target}, which is past the end of {len} instructions")]
    JumpOutOfRange {
        /// The jump instruction.
        at: usize,
        /// Where it points.
        target: u32,
        /// How many instructions there are.
        len: usize,
    },
    /// An instruction names a register the frame does not have.
    #[error("instruction {at} uses r{register}, but the frame holds {frame_size} registers")]
    RegisterOutOfRange {
        /// The instruction.
        at: usize,
        /// The register it names.
        register: u16,
        /// How many the frame has.
        frame_size: u16,
    },
    /// An instruction names a constant the pool does not have.
    #[error("instruction {at} reads k{index}, but the pool holds {len} constants")]
    ConstantOutOfRange {
        /// The instruction.
        at: usize,
        /// The index it reads.
        index: u32,
        /// How many constants there are.
        len: usize,
    },
    /// An instruction names a nested function that is not there.
    #[error("instruction {at} closes over fn{index}, but there are {len} nested blueprints")]
    BlueprintOutOfRange {
        /// The instruction.
        at: usize,
        /// The index it names.
        index: u32,
        /// How many nested blueprints there are.
        len: usize,
    },
    /// An instruction names an inline cache slot that was never allocated.
    #[error("instruction {at} uses ic{index}, but {slots} cache slots were allocated")]
    CacheOutOfRange {
        /// The instruction.
        at: usize,
        /// The slot it uses.
        index: u32,
        /// How many were allocated.
        slots: u32,
    },
    /// Control runs off the end of the function.
    #[error("the code does not end in a return or a jump")]
    FallsOffTheEnd,
}

/// Everything the runtime knows about one function before it has ever been called.
///
/// Named a blueprint rather than a code object because it is the thing a closure is stamped out of,
/// and because several closures share one. See `spec/04-frontend.md`.
///
/// A blueprint owns the blueprints of the functions written inside it, so handing one to a realm
/// hands over the whole tree and there is no separate table to keep in step with it. That is also
/// what makes the on disk cache format one object rather than an archive.
#[derive(Clone, Debug, Default)]
pub struct FunctionBlueprint {
    /// The source name, for stack traces. Empty for an anonymous function.
    pub name: String,
    /// The byte offset the function starts at, for the frame line in a stack trace.
    pub source_offset: u32,
    /// How many virtual registers a frame for this function needs.
    pub frame_size: u16,
    /// Declared parameter count, before rest and default handling.
    pub arity: u16,
    /// How many cells this function's environment holds, and zero if it needs no environment.
    ///
    /// Comes straight from scope analysis, which is the pass that knows which bindings a nested
    /// function reads. A zero here is what makes a closure chain shorter than the function nesting.
    pub cell_slots: u16,
    /// Whether the body runs in strict mode.
    pub strict: bool,
    /// The instruction stream.
    pub code: Vec<Op>,
    /// The constants the instruction stream refers to by index.
    pub constants: ConstantPool,
    /// Which byte of the source each instruction came from.
    pub positions: SourcePositions,
    /// The functions written inside this one, in the order lowering reached them.
    pub blueprints: Vec<FunctionBlueprint>,
    /// How many inline cache slots this function's sites need.
    pub cache_slots: u32,
}

impl FunctionBlueprint {
    /// The number of instructions in this blueprint.
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether this blueprint has no instructions at all, which only happens before lowering.
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Check that every index in the code points at something that exists.
    ///
    /// This is not a type checker for JavaScript and it is not trying to be. It catches the specific
    /// mistakes a lowering pass makes: a forward jump left unpatched, a register past the frame it
    /// sized, a constant index from the wrong pool. Every one of those produces bytecode that runs
    /// and is wrong, so the check runs in every lowering test rather than only in debug builds.
    pub fn verify(&self) -> Result<(), BlueprintError> {
        for (at, op) in self.code.iter().enumerate() {
            if let Some(CodeOffset(target)) = op.jump_target()
                && target as usize >= self.code.len()
            {
                return Err(BlueprintError::JumpOutOfRange {
                    at,
                    target,
                    len: self.code.len(),
                });
            }
            if let Some(ConstIndex(index)) = op.constant()
                && index as usize >= self.constants.len()
            {
                return Err(BlueprintError::ConstantOutOfRange {
                    at,
                    index,
                    len: self.constants.len(),
                });
            }
            if let Op::NewClosure {
                blueprint: BlueprintIndex(index),
                ..
            } = *op
                && index as usize >= self.blueprints.len()
            {
                return Err(BlueprintError::BlueprintOutOfRange {
                    at,
                    index,
                    len: self.blueprints.len(),
                });
            }
            if let Some(CacheIndex(index)) = cache_of(*op)
                && index >= self.cache_slots
            {
                return Err(BlueprintError::CacheOutOfRange {
                    at,
                    index,
                    slots: self.cache_slots,
                });
            }
            if let Some(Register(register)) = highest_register(*op)
                && register >= self.frame_size
            {
                return Err(BlueprintError::RegisterOutOfRange {
                    at,
                    register,
                    frame_size: self.frame_size,
                });
            }
        }

        match self.code.last() {
            Some(op) if op.is_terminator() => {}
            _ => return Err(BlueprintError::FallsOffTheEnd),
        }

        for nested in &self.blueprints {
            nested.verify()?;
        }
        Ok(())
    }

    /// A readable listing of the code, including every function written inside it.
    ///
    /// The reason this exists is that a lowering test that asserts on a listing says what it means,
    /// while one that asserts on a vector of enum variants is unreadable at the exact moment it
    /// fails. It is also the first thing anybody debugging a wrong answer wants to look at.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        self.write_listing(&mut out, 0)
            .expect("writing to a String");
        out
    }

    fn write_listing(&self, out: &mut String, depth: usize) -> fmt::Result {
        use fmt::Write as _;

        let indent = "  ".repeat(depth);
        let name = if self.name.is_empty() {
            "<anonymous>"
        } else {
            &self.name
        };
        writeln!(
            out,
            "{indent}function {name}: {} params, {} registers, {} cells, {} caches",
            self.arity, self.frame_size, self.cell_slots, self.cache_slots
        )?;

        for (at, op) in self.code.iter().enumerate() {
            let offset = self
                .positions
                .offset_at(at)
                .map_or_else(|| "-".to_owned(), |offset| offset.to_string());
            write!(out, "{indent}  {at:>4}  {offset:>6}  {op}")?;
            if let Some(index) = op.constant()
                && let Some(constant) = self.constants.get(index)
            {
                write!(out, "  ; {constant}")?;
            }
            writeln!(out)?;
        }

        for (index, nested) in self.blueprints.iter().enumerate() {
            writeln!(out)?;
            writeln!(out, "{indent}  fn{index}:")?;
            nested.write_listing(out, depth + 1)?;
        }
        Ok(())
    }
}

impl fmt::Display for FunctionBlueprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.disassemble())
    }
}

/// The inline cache slot an instruction uses, if it has one.
///
/// A free function rather than a method on `Op` because nothing outside verification wants it. The
/// interpreter reads the operand it already matched on and does not ask this question.
fn cache_of(op: Op) -> Option<CacheIndex> {
    match op {
        Op::LoadGlobal { cache, .. }
        | Op::LoadGlobalForTypeof { cache, .. }
        | Op::StoreGlobal { cache, .. }
        | Op::Add { cache, .. }
        | Op::Sub { cache, .. }
        | Op::Mul { cache, .. }
        | Op::Div { cache, .. }
        | Op::Rem { cache, .. }
        | Op::Pow { cache, .. }
        | Op::BitOr { cache, .. }
        | Op::BitXor { cache, .. }
        | Op::BitAnd { cache, .. }
        | Op::Shl { cache, .. }
        | Op::Shr { cache, .. }
        | Op::UnsignedShr { cache, .. }
        | Op::Equal { cache, .. }
        | Op::NotEqual { cache, .. }
        | Op::StrictEqual { cache, .. }
        | Op::StrictNotEqual { cache, .. }
        | Op::Less { cache, .. }
        | Op::LessEqual { cache, .. }
        | Op::Greater { cache, .. }
        | Op::GreaterEqual { cache, .. }
        | Op::In { cache, .. }
        | Op::InstanceOf { cache, .. }
        | Op::Neg { cache, .. }
        | Op::BitNot { cache, .. }
        | Op::ToNumber { cache, .. }
        | Op::Inc { cache, .. }
        | Op::Dec { cache, .. }
        | Op::GetProp { cache, .. }
        | Op::SetProp { cache, .. }
        | Op::GetIndex { cache, .. }
        | Op::SetIndex { cache, .. }
        | Op::Call { cache, .. }
        | Op::CallMethod { cache, .. }
        | Op::LoopBackEdge { profile: cache, .. } => Some(cache),
        _ => None,
    }
}

/// The largest register an instruction names, which is the one worth checking against the frame.
///
/// A call names a run of argument registers rather than one, so the last of that run is what has to
/// fit, and getting that wrong is how a frame ends up one slot short exactly when a function is
/// called with the maximum number of arguments.
#[allow(clippy::too_many_lines)]
fn highest_register(op: Op) -> Option<Register> {
    let registers: SmallVec<[Register; 3]> = match op {
        Op::LoadConst { dst, .. }
        | Op::LoadInt { dst, .. }
        | Op::LoadUndefined { dst }
        | Op::LoadNull { dst }
        | Op::LoadBool { dst, .. }
        | Op::LoadThis { dst }
        | Op::LoadClosure { dst }
        | Op::LoadUninitialized { dst }
        | Op::LoadUpvalue { dst, .. }
        | Op::LoadGlobal { dst, .. }
        | Op::LoadGlobalForTypeof { dst, .. }
        | Op::NewClosure { dst, .. } => smallvec![dst],
        Op::Move { dst, src }
        | Op::Neg { dst, src, .. }
        | Op::BitNot { dst, src, .. }
        | Op::ToNumber { dst, src, .. }
        | Op::Inc { dst, src, .. }
        | Op::Dec { dst, src, .. }
        | Op::Not { dst, src }
        | Op::TypeOf { dst, src } => smallvec![dst, src],
        Op::ThrowIfUninitialized { src, .. }
        | Op::StoreUpvalue { src, .. }
        | Op::StoreGlobal { src, .. }
        | Op::Return { src }
        | Op::JumpIfTrue { cond: src, .. }
        | Op::JumpIfFalse { cond: src, .. } => smallvec![src],
        Op::Add { dst, lhs, rhs, .. }
        | Op::Sub { dst, lhs, rhs, .. }
        | Op::Mul { dst, lhs, rhs, .. }
        | Op::Div { dst, lhs, rhs, .. }
        | Op::Rem { dst, lhs, rhs, .. }
        | Op::Pow { dst, lhs, rhs, .. }
        | Op::BitOr { dst, lhs, rhs, .. }
        | Op::BitXor { dst, lhs, rhs, .. }
        | Op::BitAnd { dst, lhs, rhs, .. }
        | Op::Shl { dst, lhs, rhs, .. }
        | Op::Shr { dst, lhs, rhs, .. }
        | Op::UnsignedShr { dst, lhs, rhs, .. }
        | Op::Equal { dst, lhs, rhs, .. }
        | Op::NotEqual { dst, lhs, rhs, .. }
        | Op::StrictEqual { dst, lhs, rhs, .. }
        | Op::StrictNotEqual { dst, lhs, rhs, .. }
        | Op::Less { dst, lhs, rhs, .. }
        | Op::LessEqual { dst, lhs, rhs, .. }
        | Op::Greater { dst, lhs, rhs, .. }
        | Op::GreaterEqual { dst, lhs, rhs, .. }
        | Op::In { dst, lhs, rhs, .. }
        | Op::InstanceOf { dst, lhs, rhs, .. } => smallvec![dst, lhs, rhs],
        Op::GetProp { dst, obj, .. } | Op::DeleteProp { dst, obj, .. } => smallvec![dst, obj],
        Op::SetProp { obj, value, .. } => smallvec![obj, value],
        Op::GetIndex {
            dst, obj, index, ..
        }
        | Op::DeleteIndex { dst, obj, index } => smallvec![dst, obj, index],
        Op::SetIndex {
            obj, index, value, ..
        } => smallvec![obj, index, value],
        Op::Call {
            dst,
            callee,
            args,
            argc,
            ..
        } => smallvec![dst, callee, last_argument(args, argc)],
        Op::CallMethod {
            dst,
            obj,
            args,
            argc,
            ..
        } => smallvec![dst, obj, last_argument(args, argc)],
        Op::ThrowConstAssignment
        | Op::NewContext { .. }
        | Op::Jump { .. }
        | Op::LoopBackEdge { .. } => SmallVec::new(),
    };
    registers.into_iter().max()
}

/// The last register in a run of `argc` arguments starting at `args`.
///
/// A call with no arguments still names a start register, and that register is where the callee's
/// arguments would go, so it has to exist in the frame even when nothing is written to it.
fn last_argument(first: Register, count: u16) -> Register {
    Register(first.0.saturating_add(count.saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::{BlueprintError, FunctionBlueprint};
    use crate::constant::ConstIndex;
    use crate::op::{BlueprintIndex, CacheIndex, CodeOffset, Op, Register};

    fn returning_a_constant() -> FunctionBlueprint {
        let mut blueprint = FunctionBlueprint {
            name: "answer".to_owned(),
            frame_size: 1,
            ..FunctionBlueprint::default()
        };
        let index = blueprint.constants.number(42.0);
        blueprint.code.push(Op::LoadConst {
            dst: Register(0),
            src: index,
        });
        blueprint.positions.record(0, 9);
        blueprint.code.push(Op::Return { src: Register(0) });
        blueprint.positions.record(1, 2);
        blueprint
    }

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
    fn a_well_formed_blueprint_verifies() {
        returning_a_constant().verify().expect("should be valid");
    }

    #[test]
    fn a_disassembly_names_the_function_and_resolves_its_constants() {
        let listing = returning_a_constant().disassemble();
        let lines: Vec<&str> = listing.lines().collect();

        assert_eq!(
            lines[0],
            "function answer: 0 params, 1 registers, 0 cells, 0 caches"
        );
        assert_eq!(lines[1].trim(), "0       9  load_const r0, k0  ; 42");
        assert_eq!(lines[2].trim(), "1       2  return r0");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn an_unpatched_forward_jump_is_caught() {
        // The bug this exists for. A jump emitted with a placeholder target and never patched points
        // at `u32::MAX`, which is a wild jump at run time and a clear message here.
        let mut blueprint = returning_a_constant();
        blueprint.code.insert(
            0,
            Op::Jump {
                target: CodeOffset(u32::MAX),
            },
        );

        let error = blueprint.verify().expect_err("should be refused");
        assert!(matches!(
            error,
            BlueprintError::JumpOutOfRange { at: 0, .. }
        ));
    }

    #[test]
    fn a_register_past_the_end_of_the_frame_is_caught() {
        let mut blueprint = returning_a_constant();
        blueprint.code.push(Op::Move {
            dst: Register(9),
            src: Register(0),
        });

        let error = blueprint.verify().expect_err("should be refused");
        assert!(matches!(
            error,
            BlueprintError::RegisterOutOfRange { register: 9, .. }
        ));
    }

    #[test]
    fn the_last_argument_of_a_call_has_to_fit_in_the_frame() {
        // Two arguments starting at r0 means r0 and r1, so a frame of one register is one short.
        let mut blueprint = returning_a_constant();
        blueprint.code.push(Op::Call {
            dst: Register(0),
            callee: Register(0),
            args: Register(0),
            argc: 2,
            cache: CacheIndex(0),
        });
        blueprint.cache_slots = 1;

        let error = blueprint.verify().expect_err("should be refused");
        assert!(matches!(
            error,
            BlueprintError::RegisterOutOfRange { register: 1, .. }
        ));
    }

    #[test]
    fn a_constant_index_from_the_wrong_pool_is_caught() {
        let mut blueprint = returning_a_constant();
        blueprint.code.insert(
            0,
            Op::LoadConst {
                dst: Register(0),
                src: ConstIndex(7),
            },
        );

        let error = blueprint.verify().expect_err("should be refused");
        assert!(matches!(
            error,
            BlueprintError::ConstantOutOfRange { index: 7, .. }
        ));
    }

    #[test]
    fn a_closure_over_a_function_that_is_not_there_is_caught() {
        let mut blueprint = returning_a_constant();
        blueprint.code.insert(
            0,
            Op::NewClosure {
                dst: Register(0),
                blueprint: BlueprintIndex(0),
            },
        );

        let error = blueprint.verify().expect_err("should be refused");
        assert!(matches!(
            error,
            BlueprintError::BlueprintOutOfRange { index: 0, .. }
        ));
    }

    #[test]
    fn a_cache_slot_that_was_never_allocated_is_caught() {
        let mut blueprint = returning_a_constant();
        blueprint.code.insert(
            0,
            Op::Add {
                dst: Register(0),
                lhs: Register(0),
                rhs: Register(0),
                cache: CacheIndex(3),
            },
        );

        let error = blueprint.verify().expect_err("should be refused");
        assert!(matches!(
            error,
            BlueprintError::CacheOutOfRange { index: 3, .. }
        ));
    }

    #[test]
    fn code_that_runs_off_the_end_is_caught() {
        let mut blueprint = returning_a_constant();
        blueprint.code.pop();

        assert!(matches!(
            blueprint.verify().expect_err("should be refused"),
            BlueprintError::FallsOffTheEnd
        ));
    }

    #[test]
    fn a_nested_blueprint_is_verified_too() {
        // A broken function inside a valid one is exactly the case a shallow check would miss.
        let mut outer = returning_a_constant();
        let mut inner = returning_a_constant();
        inner.code.pop();
        outer.blueprints.push(inner);

        assert!(matches!(
            outer.verify().expect_err("should be refused"),
            BlueprintError::FallsOffTheEnd
        ));
    }

    #[test]
    fn a_disassembly_includes_the_functions_written_inside() {
        let mut outer = returning_a_constant();
        outer.blueprints.push(returning_a_constant());

        let listing = outer.disassemble();
        assert!(listing.contains("fn0:"), "{listing}");
        assert_eq!(listing.matches("function answer").count(), 2, "{listing}");
    }
}
