//! The dispatch loop.
//!
//! One `loop` over one `match`, which is strategy A in spec 5.3. It is the portable one, it works on
//! stable Rust today, and it is the thing the other two strategies get measured against. Tail called
//! handlers need `become`, which is nightly and targeted at 2027, and the stencil threaded loop needs
//! the tier 1 stencils that do not exist yet. Neither of those is allowed to block anything, which is
//! why the default is the one that needs nothing.
//!
//! The loop is deliberately flat. Every opcode is one arm, the arm does the work, and nothing is
//! hidden behind a generic helper that takes a closure. That costs some repetition and it buys the
//! property spec 5.2 is really asking for, which is that the semantics of an opcode are readable in
//! one place. It also means that when the quickening in 5.5 arrives, an arm can be split into a fast
//! path and a slow path without unpicking an abstraction first.
//!
//! # What runs and what does not
//!
//! Everything that happens inside a single frame: loads, moves, the arithmetic and comparison
//! operators, the unary operators, the dead zone checks, jumps, back edges and `return`. That is the
//! third item on the M0 checklist and it is enough to run a program that computes something.
//!
//! Calls, closures, environments, globals and property access are not here. They need a heap object
//! with a header to point at and M0 has a bump allocator with no object model on top of it yet, so
//! they land in the next piece of work rather than being faked here. Every one of them reaches the
//! same arm at the bottom of the match and produces an error that names the opcode, which is a
//! refusal rather than a wrong answer.

// Same reason as in `number`. JavaScript equality on numbers is exact IEEE equality, so comparing
// two doubles with `==` is the specified behaviour and not an oversight.
#![allow(clippy::float_cmp)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use katsu_ir::{Constant, FunctionBlueprint, Op, Register};

use crate::Value;
use crate::number::{exponentiate, shift_count, to_int32, to_uint32};
use crate::stack::{Stack, StackError};

/// Why execution stopped somewhere other than a `return`.
///
/// The first three are JavaScript exceptions, which a program is allowed to catch, and they carry
/// the message Node prints for the same situation. They are Rust errors here because M0 has no `try`
/// to catch them with, and when it does the interpreter will catch them rather than propagating.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// A `ReferenceError`, which today means a dead zone check fired.
    #[error("ReferenceError: {0}")]
    Reference(String),
    /// A `TypeError`, which today means an assignment to a `const` binding.
    #[error("TypeError: {0}")]
    Type(String),
    /// A `RangeError`, which today means the stack ran out.
    #[error("RangeError: {0}")]
    Range(String),
    /// An opcode that lowering can emit and the interpreter cannot run yet.
    ///
    /// Not a panic, because the point of it is that a program using a construct we have not finished
    /// gets a clear refusal naming the opcode rather than a wrong answer or a crash.
    #[error("{0} is not implemented yet")]
    NotImplemented(Op),
    /// A string valued constant, which needs the heap that arrives with the next piece of work.
    #[error("string values are not implemented yet")]
    NoStrings,
    /// Something asked the loop to stop, which today is only ever a test.
    #[error("execution was interrupted")]
    Interrupted,
}

impl From<StackError> for RuntimeError {
    fn from(error: StackError) -> RuntimeError {
        match error {
            // The one a program sees, and it says exactly what Node says.
            StackError::Overflow => {
                RuntimeError::Range("Maximum call stack size exceeded".to_owned())
            }
            // Running out of address space at startup is not really a JavaScript error, it is the
            // process failing, and it is reported in the operating system's words rather than
            // dressed up as something a program could have caused.
            StackError::Reserve(inner) => RuntimeError::Range(inner.to_string()),
        }
    }
}

/// The word every back edge checks, in the isolate rather than in the frame.
///
/// Spec 5.6 asks for exactly one mechanism covering collection safepoints, tier up, execution
/// timeouts and worker termination, and this is it. Setting it from another thread is the point,
/// which is why it is shared and atomic rather than a plain field. Nothing in the runtime sets it
/// yet, so the only thing that does today is the test that proves an endless loop can be stopped.
#[derive(Clone, Debug, Default)]
pub struct Interrupt(Arc<AtomicU32>);

impl Interrupt {
    /// Ask whatever is running to stop at its next back edge.
    pub fn request(&self) {
        // Release, so that anything the requesting thread wrote before this is visible to the
        // interpreter thread once it observes the flag.
        self.0.store(1, Ordering::Release);
    }

    /// Clear the flag, so the same interpreter can be used again.
    pub fn clear(&self) {
        self.0.store(0, Ordering::Release);
    }

    /// Whether a stop has been asked for.
    #[must_use]
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }
}

/// One thread's worth of execution: a stack, and the flag that can stop it.
#[derive(Debug)]
pub struct Interpreter {
    stack: Stack,
    interrupt: Interrupt,
}

impl Interpreter {
    /// Create an interpreter with an empty stack.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Range`] if the stack's address space cannot be reserved, which means
    /// the process is out of address space and nothing else is going to work either.
    pub fn new() -> Result<Interpreter, RuntimeError> {
        Ok(Interpreter {
            stack: Stack::new()?,
            interrupt: Interrupt::default(),
        })
    }

    /// A handle another thread can use to stop this interpreter at its next back edge.
    #[must_use]
    pub fn interrupt(&self) -> Interrupt {
        self.interrupt.clone()
    }

    /// How deep the JavaScript stack is, which is zero unless something is running.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.depth()
    }

    /// Bytes the stack has committed, which is the interpreter's share of the memory budget.
    #[must_use]
    pub fn committed_bytes(&self) -> usize {
        self.stack.committed_bytes()
    }

    /// Run a blueprint from its first instruction and return the value it returns.
    ///
    /// The frame is popped whether the body returned or threw, so an error leaves the interpreter
    /// usable rather than leaving a dead frame behind for the next call to trip over.
    ///
    /// # Errors
    ///
    /// Returns whatever the body threw, or [`RuntimeError::NotImplemented`] if it reached an opcode
    /// this interpreter does not run yet.
    pub fn run(&mut self, blueprint: &FunctionBlueprint) -> Result<Value, RuntimeError> {
        self.stack.push(blueprint.frame_size, &[], 0, Register(0))?;
        let outcome = self.execute(blueprint);
        self.stack.pop();
        outcome
    }

    /// The loop.
    ///
    /// Long because the instruction set is long, and one arm per opcode with no shared helper is
    /// the point rather than an accident. `match_same_arms` is off for the same reason: `load_this`
    /// and `load_undefined` happen to produce the same value today and stop doing so the moment a
    /// function can be called with a receiver, so merging them would be merging two different
    /// questions that currently have the same answer.
    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    fn execute(&mut self, blueprint: &FunctionBlueprint) -> Result<Value, RuntimeError> {
        let code = &blueprint.code;
        let mut pc = 0_usize;

        loop {
            // A verified blueprint ends in a terminator and has no jump target past its end, so
            // running off the end is impossible and the index is a bug in lowering rather than in
            // the program. Panicking here would be the same claim with a worse message.
            let op = *code
                .get(pc)
                .expect("a verified blueprint ends in a terminator, so the pc stays inside it");
            pc += 1;

            match op {
                Op::LoadConst { dst, src } => {
                    let value = match blueprint.constants.get(src) {
                        Some(Constant::Number(number)) => Value::from_f64(*number),
                        Some(Constant::String(_)) => return Err(RuntimeError::NoStrings),
                        None => unreachable!("verify checked every constant index"),
                    };
                    self.stack.set(dst, value);
                }
                Op::LoadInt { dst, value } => self.stack.set(dst, Value::from_i32(value)),
                Op::LoadUndefined { dst } => self.stack.set(dst, Value::UNDEFINED),
                Op::LoadNull { dst } => self.stack.set(dst, Value::NULL),
                Op::LoadBool { dst, value } => self.stack.set(dst, Value::from_bool(value)),
                Op::Move { dst, src } => {
                    let value = self.stack.get(src);
                    self.stack.set(dst, value);
                }

                // The hole. Deliberately the empty value rather than a sentinel of its own, because
                // "there is no value here" is exactly what the dead zone means and the encoding
                // already has a way to say it.
                Op::LoadUninitialized { dst } => self.stack.set(dst, Value::EMPTY),
                Op::ThrowIfUninitialized { src, name } => {
                    if self.stack.get(src).is_empty() {
                        let name = Self::constant_name(blueprint, name);
                        return Err(RuntimeError::Reference(format!(
                            "Cannot access '{name}' before initialization"
                        )));
                    }
                }
                Op::ThrowConstAssignment => {
                    return Err(RuntimeError::Type(
                        "Assignment to constant variable.".to_owned(),
                    ));
                }

                // `this` at the top level of a module is `undefined`, and no function in M0 can be
                // called with a receiver because calls are not implemented, so there is nothing else
                // it can be yet. The CommonJS case, where it is `module.exports`, arrives with the
                // module system.
                Op::LoadThis { dst } => self.stack.set(dst, Value::UNDEFINED),

                Op::Add { dst, lhs, rhs, .. } => {
                    // String concatenation is the other half of `+` and it needs the heap, so a
                    // string operand refuses rather than silently converting to a number.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_f64(lhs + rhs));
                }
                Op::Sub { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_f64(lhs - rhs));
                }
                Op::Mul { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_f64(lhs * rhs));
                }
                Op::Div { dst, lhs, rhs, .. } => {
                    // No check for a zero divisor. JavaScript has no integer division, so this is
                    // IEEE division all the way down and `1 / 0` is `Infinity` rather than a fault.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_f64(lhs / rhs));
                }
                Op::Rem { dst, lhs, rhs, .. } => {
                    // Rust's `%` on floats truncates towards zero and keeps the sign of the
                    // dividend, which is what the specification asks for. `-5 % 3` is `-2` in both
                    // languages, and it is `1` in Python, which is the trap this comment exists for.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_f64(lhs % rhs));
                }
                Op::Pow { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_f64(exponentiate(lhs, rhs)));
                }

                Op::BitOr { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack
                        .set(dst, Value::from_i32(to_int32(lhs) | to_int32(rhs)));
                }
                Op::BitXor { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack
                        .set(dst, Value::from_i32(to_int32(lhs) ^ to_int32(rhs)));
                }
                Op::BitAnd { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack
                        .set(dst, Value::from_i32(to_int32(lhs) & to_int32(rhs)));
                }
                Op::Shl { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    let shifted = to_int32(lhs).wrapping_shl(shift_count(rhs));
                    self.stack.set(dst, Value::from_i32(shifted));
                }
                Op::Shr { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    let shifted = to_int32(lhs).wrapping_shr(shift_count(rhs));
                    self.stack.set(dst, Value::from_i32(shifted));
                }
                Op::UnsignedShr { dst, lhs, rhs, .. } => {
                    // The one shift whose result does not fit an `i32`. `-1 >>> 0` is four billion
                    // and change, so the answer goes back as a number and lets the value encoding
                    // decide how to hold it.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    let shifted = to_uint32(lhs).wrapping_shr(shift_count(rhs));
                    self.stack.set(dst, Value::from_f64(f64::from(shifted)));
                }

                Op::Equal { dst, lhs, rhs, .. } => {
                    let equal = self.loose_equal(lhs, rhs)?;
                    self.stack.set(dst, Value::from_bool(equal));
                }
                Op::NotEqual { dst, lhs, rhs, .. } => {
                    let equal = self.loose_equal(lhs, rhs)?;
                    self.stack.set(dst, Value::from_bool(!equal));
                }
                Op::StrictEqual { dst, lhs, rhs, .. } => {
                    let equal = self.strict_equal(lhs, rhs);
                    self.stack.set(dst, Value::from_bool(equal));
                }
                Op::StrictNotEqual { dst, lhs, rhs, .. } => {
                    let equal = self.strict_equal(lhs, rhs);
                    self.stack.set(dst, Value::from_bool(!equal));
                }

                // Rust's float comparisons are the IEEE ones, so every one of these is already false
                // when either side is a NaN, which is what the specification asks for. Writing them
                // as a `partial_cmp` and a match would be a longer way of saying the same thing and
                // an easier one to get wrong.
                Op::Less { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_bool(lhs < rhs));
                }
                Op::LessEqual { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_bool(lhs <= rhs));
                }
                Op::Greater { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_bool(lhs > rhs));
                }
                Op::GreaterEqual { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs)?;
                    self.stack.set(dst, Value::from_bool(lhs >= rhs));
                }

                Op::Neg { dst, src, .. } => {
                    let number = self.number(src)?;
                    self.stack.set(dst, Value::from_f64(-number));
                }
                Op::BitNot { dst, src, .. } => {
                    let number = self.number(src)?;
                    self.stack.set(dst, Value::from_i32(!to_int32(number)));
                }
                Op::ToNumber { dst, src, .. } => {
                    let number = self.number(src)?;
                    self.stack.set(dst, Value::from_f64(number));
                }
                Op::Inc { dst, src, .. } => {
                    let number = self.number(src)?;
                    self.stack.set(dst, Value::from_f64(number + 1.0));
                }
                Op::Dec { dst, src, .. } => {
                    let number = self.number(src)?;
                    self.stack.set(dst, Value::from_f64(number - 1.0));
                }
                Op::Not { dst, src } => {
                    let truthy = self.stack.get(src).to_boolean();
                    self.stack.set(dst, Value::from_bool(!truthy));
                }
                // The answer to `typeof` is a string and `Value::type_of` already knows which one,
                // so this is the one arm where the work is finished and the result is the thing
                // that cannot be represented yet.
                Op::TypeOf { .. } => return Err(RuntimeError::NoStrings),

                Op::Jump { target } => pc = target.0 as usize,
                Op::JumpIfTrue { cond, target } => {
                    if self.stack.get(cond).to_boolean() {
                        pc = target.0 as usize;
                    }
                }
                Op::JumpIfFalse { cond, target } => {
                    if !self.stack.get(cond).to_boolean() {
                        pc = target.0 as usize;
                    }
                }
                Op::LoopBackEdge { target, .. } => {
                    // One load and one compare per iteration, which is what spec 5.6 asks for and
                    // is the only mechanism covering collection safepoints, timeouts, tier up and
                    // worker termination. The tier up counter hangs off the same edge and arrives
                    // with tier 1.
                    if self.interrupt.requested() {
                        return Err(RuntimeError::Interrupted);
                    }
                    pc = target.0 as usize;
                }

                Op::Return { src } => return Ok(self.stack.get(src)),

                // Everything that needs a heap object to point at, which is calls, closures,
                // environments, globals and property access. Named rather than silently wrong.
                _ => return Err(RuntimeError::NotImplemented(op)),
            }
        }
    }

    /// The name a constant holds, for an error message.
    fn constant_name(blueprint: &FunctionBlueprint, index: katsu_ir::ConstIndex) -> String {
        match blueprint.constants.get(index) {
            Some(Constant::String(name)) => name.to_string(),
            // Lowering only ever puts a string in a name operand, so this is unreachable in practice
            // and is a placeholder rather than a panic, because failing to name a variable is not
            // worth losing the error that was actually being reported.
            _ => "<unknown>".to_owned(),
        }
    }

    /// `ToNumber` of one register, for the operators that take a number.
    ///
    /// Refuses anything that would need `ToPrimitive`, which is every heap value, because that runs
    /// `valueOf` and there is nothing to run it on yet.
    fn number(&self, register: Register) -> Result<f64, RuntimeError> {
        let value = self.stack.get(register);
        if let Some(number) = value.as_f64() {
            return Ok(number);
        }
        if value.is_undefined() {
            return Ok(f64::NAN);
        }
        if value.is_null() {
            return Ok(0.0);
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { 1.0 } else { 0.0 });
        }
        Err(RuntimeError::NoStrings)
    }

    /// `ToNumber` of both operands of a binary operator, left to right.
    ///
    /// The order matters and is not an accident. Once these can call `valueOf`, the left operand's
    /// conversion runs first and can throw or have a side effect, and a program can see which one
    /// happened.
    fn two_numbers(&self, lhs: Register, rhs: Register) -> Result<(f64, f64), RuntimeError> {
        let lhs = self.number(lhs)?;
        let rhs = self.number(rhs)?;
        Ok((lhs, rhs))
    }

    /// `===`, which is a type check and then a value check.
    ///
    /// Numbers compare as numbers rather than as bit patterns, because `1` and `1.0` are the same
    /// JavaScript number held two different ways, and because `NaN === NaN` is false while the two
    /// have identical bits.
    fn strict_equal(&self, lhs: Register, rhs: Register) -> bool {
        let lhs = self.stack.get(lhs);
        let rhs = self.stack.get(rhs);
        if let (Some(lhs), Some(rhs)) = (lhs.as_f64(), rhs.as_f64()) {
            return lhs == rhs;
        }
        // Everything else that exists in M0 is an immediate, so identical bits and identical values
        // are the same question. That stops being true when a heap value can be reached by two
        // different pointers, which is a change to make when there is such a thing.
        lhs == rhs
    }

    /// `==`, for the types that exist.
    ///
    /// The only coercions reachable today are the nullish pair, which are equal to each other and to
    /// nothing else, and a boolean against a number, which converts the boolean. Everything that
    /// would need `ToPrimitive` refuses.
    fn loose_equal(&self, lhs: Register, rhs: Register) -> Result<bool, RuntimeError> {
        let left = self.stack.get(lhs);
        let right = self.stack.get(rhs);

        // Checked before anything else, because `null == undefined` is true and neither of them is
        // equal to anything else at all, not even to zero or to the empty string.
        if left.is_nullish() || right.is_nullish() {
            return Ok(left.is_nullish() && right.is_nullish());
        }
        if left.is_number() && right.is_number() {
            return Ok(self.strict_equal(lhs, rhs));
        }
        if left.is_bool() && right.is_bool() {
            return Ok(left == right);
        }
        // A boolean against a number converts the boolean and compares as numbers, which is why
        // `1 == true` is true and `2 == true` is false.
        if left.is_bool() && right.is_number() || left.is_number() && right.is_bool() {
            let left = self.number(lhs)?;
            let right = self.number(rhs)?;
            return Ok(left == right);
        }
        Err(RuntimeError::NoStrings)
    }
}

#[cfg(test)]
mod tests {
    use katsu_ir::{
        CacheIndex, CodeOffset, ConstIndex, ConstantPool, FunctionBlueprint, Op, Register,
    };

    use super::{Interpreter, RuntimeError};
    use crate::Value;

    /// Every test frame is this big, which is more registers than any test here uses.
    ///
    /// A fixed size rather than one computed from the code, because working out the highest register
    /// an instruction names is exactly what `verify` already does, and a helper that got it wrong
    /// would fail the tests for a reason that has nothing to do with the loop.
    const FRAME: u16 = 8;

    /// The one inline cache slot the operators in these tests share. Nothing reads it yet.
    const IC: CacheIndex = CacheIndex(0);

    /// Assemble instructions into a blueprint and verify it.
    ///
    /// The tests here are about the loop and not about the frontend, so they say which instructions
    /// run rather than which source produced them. That is more precise, it does not break when
    /// lowering changes which register it picked, and it can express a sequence lowering has no
    /// syntax for yet. Every one of them still goes through `verify`, so a test cannot accidentally
    /// assert on bytecode that is not well formed.
    fn assemble(code: Vec<Op>, constants: ConstantPool) -> FunctionBlueprint {
        let blueprint = FunctionBlueprint {
            frame_size: FRAME,
            cache_slots: 1,
            code,
            constants,
            ..FunctionBlueprint::default()
        };
        blueprint.verify().expect("the test assembled bad bytecode");
        blueprint
    }

    /// Run a sequence of instructions and return what it returned.
    fn run(code: Vec<Op>) -> Result<Value, RuntimeError> {
        run_with(code, ConstantPool::default())
    }

    fn run_with(code: Vec<Op>, constants: ConstantPool) -> Result<Value, RuntimeError> {
        let blueprint = assemble(code, constants);
        Interpreter::new()
            .expect("should reserve a stack")
            .run(&blueprint)
    }

    /// The number a sequence returned, which is what most of these assert on.
    #[track_caller]
    fn number(code: Vec<Op>) -> f64 {
        run(code)
            .expect("should not throw")
            .as_f64()
            .expect("should return a number")
    }

    /// The boolean a sequence returned.
    #[track_caller]
    fn boolean(code: Vec<Op>) -> bool {
        run(code)
            .expect("should not throw")
            .as_bool()
            .expect("should return a boolean")
    }

    /// `r0 = a; r1 = b; r2 = a op b; return r2`, which is the shape of every binary test here.
    fn binary(a: i32, b: i32, op: fn(Register, Register, Register, CacheIndex) -> Op) -> Vec<Op> {
        vec![
            Op::LoadInt {
                dst: Register(0),
                value: a,
            },
            Op::LoadInt {
                dst: Register(1),
                value: b,
            },
            op(Register(2), Register(0), Register(1), IC),
            Op::Return { src: Register(2) },
        ]
    }

    #[test]
    fn a_loaded_value_comes_back_out_of_a_return() {
        assert_eq!(
            run(vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 7,
                },
                Op::Return { src: Register(0) },
            ]),
            Ok(Value::from_i32(7))
        );
    }

    #[test]
    fn the_immediates_each_load_as_themselves() {
        for (op, expected) in [
            (Op::LoadUndefined { dst: Register(0) }, Value::UNDEFINED),
            (Op::LoadNull { dst: Register(0) }, Value::NULL),
            (
                Op::LoadBool {
                    dst: Register(0),
                    value: true,
                },
                Value::TRUE,
            ),
            (
                Op::LoadBool {
                    dst: Register(0),
                    value: false,
                },
                Value::FALSE,
            ),
            // `this` at the top level of a module, which is the only receiver M0 can produce.
            (Op::LoadThis { dst: Register(0) }, Value::UNDEFINED),
        ] {
            assert_eq!(
                run(vec![op, Op::Return { src: Register(0) }]),
                Ok(expected),
                "{op}"
            );
        }
    }

    #[test]
    fn a_number_that_does_not_fit_an_immediate_comes_from_the_pool() {
        let mut constants = ConstantPool::default();
        let index = constants.number(1.5);
        assert_eq!(
            run_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: index,
                    },
                    Op::Return { src: Register(0) },
                ],
                constants
            ),
            Ok(Value::from_double(1.5))
        );
    }

    #[test]
    fn a_move_copies_and_does_not_alias() {
        assert_eq!(
            run(vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                },
                Op::Move {
                    dst: Register(1),
                    src: Register(0),
                },
                Op::LoadInt {
                    dst: Register(0),
                    value: 2,
                },
                Op::Return { src: Register(1) },
            ]),
            Ok(Value::from_i32(1))
        );
    }

    #[test]
    fn arithmetic_is_the_arithmetic_the_specification_describes() {
        assert_eq!(number(binary(2, 3, add)), 5.0);
        assert_eq!(number(binary(2, 3, sub)), -1.0);
        assert_eq!(number(binary(6, 7, mul)), 42.0);
        assert_eq!(number(binary(7, 2, div)), 3.5);
        // Truncating towards zero and keeping the dividend's sign, which is what JavaScript says and
        // is not what a language with a floor modulo would say.
        assert_eq!(number(binary(-5, 3, rem)), -2.0);
        assert_eq!(number(binary(2, 10, pow)), 1024.0);
    }

    #[test]
    fn dividing_by_zero_is_infinity_and_not_a_fault() {
        // JavaScript has no integer division, so this is IEEE all the way down.
        assert_eq!(number(binary(1, 0, div)), f64::INFINITY);
        assert_eq!(number(binary(-1, 0, div)), f64::NEG_INFINITY);
        assert!(number(binary(0, 0, div)).is_nan());
    }

    #[test]
    fn the_bitwise_operators_go_through_the_thirty_two_bit_wrap() {
        assert_eq!(number(binary(12, 10, bit_or)), 14.0);
        assert_eq!(number(binary(12, 10, bit_and)), 8.0);
        assert_eq!(number(binary(12, 10, bit_xor)), 6.0);
        assert_eq!(number(binary(1, 4, shl)), 16.0);
        assert_eq!(number(binary(-16, 2, shr)), -4.0);
        // The one whose result does not fit a signed thirty two bit integer.
        assert_eq!(number(binary(-1, 0, unsigned_shr)), 4_294_967_295.0);
        // And the shift count that wraps, which is why `1 << 32` is one.
        assert_eq!(number(binary(1, 32, shl)), 1.0);
    }

    #[test]
    fn comparison_is_false_on_both_sides_when_a_nan_is_involved() {
        // The property that makes `a < b` and `!(a >= b)` different questions, and the one a hand
        // written comparison gets wrong first.
        let mut constants = ConstantPool::default();
        let nan = constants.number(f64::NAN);
        for build in [less, less_equal, greater, greater_equal] {
            let op = build(Register(2), Register(0), Register(1), IC);
            let result = run_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: nan,
                    },
                    Op::LoadInt {
                        dst: Register(1),
                        value: 1,
                    },
                    op,
                    Op::Return { src: Register(2) },
                ],
                constants.clone(),
            );
            assert_eq!(result, Ok(Value::FALSE), "{op} said true about a NaN");
        }
    }

    #[test]
    fn comparison_is_otherwise_ordinary() {
        assert!(boolean(binary(1, 2, less)));
        assert!(!boolean(binary(2, 2, less)));
        assert!(boolean(binary(2, 2, less_equal)));
        assert!(boolean(binary(3, 2, greater)));
        assert!(boolean(binary(2, 2, greater_equal)));
    }

    #[test]
    fn strict_equality_compares_numbers_as_numbers_and_not_as_bit_patterns() {
        // `1` and `1.0` are the same JavaScript number held two different ways, and their bits
        // differ, so a bit compare would get this wrong in the direction that is hard to notice.
        let mut constants = ConstantPool::default();
        let one = constants.number(1.0);
        let compare = |src: ConstIndex| {
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                },
                Op::LoadConst {
                    dst: Register(1),
                    src,
                },
                Op::StrictEqual {
                    dst: Register(2),
                    lhs: Register(0),
                    rhs: Register(1),
                    cache: IC,
                },
                Op::Return { src: Register(2) },
            ]
        };
        assert_eq!(run_with(compare(one), constants.clone()), Ok(Value::TRUE));

        // And a NaN, which has identical bits to itself and is not equal to itself.
        let nan = constants.number(f64::NAN);
        assert_eq!(
            run_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: nan,
                    },
                    Op::Move {
                        dst: Register(1),
                        src: Register(0),
                    },
                    Op::StrictEqual {
                        dst: Register(2),
                        lhs: Register(0),
                        rhs: Register(1),
                        cache: IC,
                    },
                    Op::Return { src: Register(2) },
                ],
                constants
            ),
            Ok(Value::FALSE)
        );
    }

    #[test]
    fn strict_equality_does_not_convert_across_types() {
        // `1 == true` is true and `1 === true` is false, which is the whole difference.
        assert!(!boolean(vec![
            Op::LoadInt {
                dst: Register(0),
                value: 1,
            },
            Op::LoadBool {
                dst: Register(1),
                value: true,
            },
            Op::StrictEqual {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                cache: IC,
            },
            Op::Return { src: Register(2) },
        ]));
        assert!(boolean(vec![
            Op::LoadNull { dst: Register(0) },
            Op::LoadUndefined { dst: Register(1) },
            Op::StrictNotEqual {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                cache: IC,
            },
            Op::Return { src: Register(2) },
        ]));
    }

    #[test]
    fn null_and_undefined_are_loosely_equal_to_each_other_and_to_nothing_else() {
        let cases = [
            (
                Op::LoadNull { dst: Register(0) },
                Op::LoadUndefined { dst: Register(1) },
                true,
            ),
            (
                Op::LoadNull { dst: Register(0) },
                Op::LoadNull { dst: Register(1) },
                true,
            ),
            (
                Op::LoadUndefined { dst: Register(0) },
                Op::LoadUndefined { dst: Register(1) },
                true,
            ),
            // The one people expect to be true because `null` is falsy, and is not.
            (
                Op::LoadNull { dst: Register(0) },
                Op::LoadInt {
                    dst: Register(1),
                    value: 0,
                },
                false,
            ),
            (
                Op::LoadNull { dst: Register(0) },
                Op::LoadBool {
                    dst: Register(1),
                    value: false,
                },
                false,
            ),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                boolean(vec![
                    left,
                    right,
                    Op::Equal {
                        dst: Register(2),
                        lhs: Register(0),
                        rhs: Register(1),
                        cache: IC,
                    },
                    Op::Return { src: Register(2) },
                ]),
                expected,
                "{left} == {right}"
            );
        }
    }

    #[test]
    fn a_boolean_against_a_number_converts_the_boolean() {
        for (left, right, expected) in [(1, true, true), (2, true, false), (0, false, true)] {
            assert_eq!(
                boolean(vec![
                    Op::LoadInt {
                        dst: Register(0),
                        value: left,
                    },
                    Op::LoadBool {
                        dst: Register(1),
                        value: right,
                    },
                    Op::Equal {
                        dst: Register(2),
                        lhs: Register(0),
                        rhs: Register(1),
                        cache: IC,
                    },
                    Op::Return { src: Register(2) },
                ]),
                expected,
                "{left} == {right}"
            );
        }
    }

    #[test]
    fn not_equal_is_the_negation_and_not_a_second_implementation() {
        assert!(boolean(vec![
            Op::LoadNull { dst: Register(0) },
            Op::LoadInt {
                dst: Register(1),
                value: 0,
            },
            Op::NotEqual {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                cache: IC,
            },
            Op::Return { src: Register(2) },
        ]));
    }

    #[test]
    fn the_unary_operators_convert_before_they_operate() {
        // `-null` is `-0` and `~undefined` is `-1`, both of which go through ToNumber first.
        assert_eq!(
            run(vec![
                Op::LoadNull { dst: Register(0) },
                Op::Neg {
                    dst: Register(1),
                    src: Register(0),
                    cache: IC,
                },
                Op::Return { src: Register(1) },
            ]),
            Ok(Value::from_double(-0.0))
        );
        assert_eq!(
            number(vec![
                Op::LoadUndefined { dst: Register(0) },
                Op::BitNot {
                    dst: Register(1),
                    src: Register(0),
                    cache: IC,
                },
                Op::Return { src: Register(1) },
            ]),
            -1.0
        );
        assert!(
            number(vec![
                Op::LoadUndefined { dst: Register(0) },
                Op::ToNumber {
                    dst: Register(1),
                    src: Register(0),
                    cache: IC,
                },
                Op::Return { src: Register(1) },
            ])
            .is_nan()
        );
    }

    #[test]
    fn increment_and_decrement_work_on_things_that_are_not_numbers_yet() {
        assert_eq!(
            number(vec![
                Op::LoadBool {
                    dst: Register(0),
                    value: true,
                },
                Op::Inc {
                    dst: Register(1),
                    src: Register(0),
                    cache: IC,
                },
                Op::Return { src: Register(1) },
            ]),
            2.0
        );
        assert_eq!(
            number(vec![
                Op::LoadNull { dst: Register(0) },
                Op::Dec {
                    dst: Register(1),
                    src: Register(0),
                    cache: IC,
                },
                Op::Return { src: Register(1) },
            ]),
            -1.0
        );
    }

    #[test]
    fn not_follows_the_truthiness_rules_and_not_the_type() {
        let cases = [
            (
                Op::LoadInt {
                    dst: Register(0),
                    value: 0,
                },
                true,
            ),
            (
                Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                },
                false,
            ),
            (Op::LoadNull { dst: Register(0) }, true),
            (Op::LoadUndefined { dst: Register(0) }, true),
            (
                Op::LoadBool {
                    dst: Register(0),
                    value: false,
                },
                true,
            ),
        ];
        for (load, expected) in cases {
            assert_eq!(
                boolean(vec![
                    load,
                    Op::Not {
                        dst: Register(1),
                        src: Register(0),
                    },
                    Op::Return { src: Register(1) },
                ]),
                expected,
                "not {load}"
            );
        }
    }

    #[test]
    fn a_jump_skips_the_instructions_it_jumps_over() {
        assert_eq!(
            run(vec![
                Op::Jump {
                    target: CodeOffset(2),
                },
                Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                },
                Op::LoadInt {
                    dst: Register(0),
                    value: 2,
                },
                Op::Return { src: Register(0) },
            ]),
            Ok(Value::from_i32(2))
        );
    }

    #[test]
    fn a_conditional_jump_asks_for_truthiness_and_not_for_a_boolean() {
        let branch = |value: i32| {
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value,
                },
                Op::JumpIfFalse {
                    cond: Register(0),
                    target: CodeOffset(4),
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 10,
                },
                Op::Return { src: Register(1) },
                Op::LoadInt {
                    dst: Register(1),
                    value: 20,
                },
                Op::Return { src: Register(1) },
            ]
        };
        assert_eq!(run(branch(1)), Ok(Value::from_i32(10)));
        assert_eq!(run(branch(0)), Ok(Value::from_i32(20)));
    }

    #[test]
    fn a_jump_if_true_takes_the_other_side_of_the_same_question() {
        assert_eq!(
            run(vec![
                Op::LoadBool {
                    dst: Register(0),
                    value: true,
                },
                Op::JumpIfTrue {
                    cond: Register(0),
                    target: CodeOffset(3),
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 1,
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 2,
                },
                Op::Return { src: Register(1) },
            ]),
            Ok(Value::from_i32(2))
        );
    }

    #[test]
    fn a_loop_runs_to_its_condition_and_stops() {
        // `let i = 0; while (i < 5) i = i + 1; return i;` written out, because a loop is the shape a
        // back edge exists for.
        assert_eq!(
            number(vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 0,
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 5,
                },
                Op::Less {
                    dst: Register(2),
                    lhs: Register(0),
                    rhs: Register(1),
                    cache: IC,
                },
                Op::JumpIfFalse {
                    cond: Register(2),
                    target: CodeOffset(7),
                },
                Op::LoadInt {
                    dst: Register(3),
                    value: 1,
                },
                Op::Add {
                    dst: Register(0),
                    lhs: Register(0),
                    rhs: Register(3),
                    cache: IC,
                },
                Op::LoopBackEdge {
                    target: CodeOffset(2),
                    profile: IC,
                },
                Op::Return { src: Register(0) },
            ]),
            5.0
        );
    }

    #[test]
    fn the_dead_zone_check_fires_on_the_hole_and_names_the_binding() {
        let mut constants = ConstantPool::default();
        let name = constants.string("x");
        let error = run_with(
            vec![
                Op::LoadUninitialized { dst: Register(0) },
                Op::ThrowIfUninitialized {
                    src: Register(0),
                    name,
                },
                Op::Return { src: Register(0) },
            ],
            constants,
        )
        .expect_err("should throw");
        assert_eq!(
            error.to_string(),
            "ReferenceError: Cannot access 'x' before initialization"
        );
    }

    #[test]
    fn the_dead_zone_check_passes_once_something_has_been_written() {
        let mut constants = ConstantPool::default();
        let name = constants.string("x");
        assert_eq!(
            run_with(
                vec![
                    Op::LoadUninitialized { dst: Register(0) },
                    Op::LoadInt {
                        dst: Register(0),
                        value: 1,
                    },
                    Op::ThrowIfUninitialized {
                        src: Register(0),
                        name,
                    },
                    Op::Return { src: Register(0) },
                ],
                constants
            ),
            Ok(Value::from_i32(1))
        );
    }

    #[test]
    fn assigning_to_a_constant_throws_what_node_throws() {
        let error = run(vec![Op::ThrowConstAssignment]).expect_err("should throw");
        assert_eq!(
            error.to_string(),
            "TypeError: Assignment to constant variable."
        );
    }

    #[test]
    fn an_opcode_that_is_not_implemented_names_itself_rather_than_guessing() {
        let error = run(vec![
            Op::LoadInt {
                dst: Register(0),
                value: 1,
            },
            Op::GetIndex {
                dst: Register(1),
                obj: Register(0),
                index: Register(0),
                cache: IC,
            },
            Op::Return { src: Register(1) },
        ])
        .expect_err("should refuse");
        assert!(
            error.to_string().starts_with("get_index"),
            "the error should name the opcode, and it said {error}"
        );
    }

    #[test]
    fn an_endless_loop_stops_when_something_asks_it_to() {
        // The mechanism from spec 5.6, and the reason the check is on the back edge rather than
        // somewhere convenient. Without it this test does not terminate, which is the point.
        let blueprint = assemble(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 0,
                },
                Op::LoopBackEdge {
                    target: CodeOffset(0),
                    profile: IC,
                },
            ],
            ConstantPool::default(),
        );
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let interrupt = interpreter.interrupt();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            interrupt.request();
        });
        assert_eq!(interpreter.run(&blueprint), Err(RuntimeError::Interrupted));

        // And it can be used again once the flag is cleared, which is what a collection safepoint
        // will need and what a timeout will not.
        interpreter.interrupt().clear();
        assert!(!interpreter.interrupt().requested());
    }

    #[test]
    fn a_throw_leaves_the_interpreter_usable() {
        // The frame is popped on the way out, so the next call starts from a clean stack rather than
        // on top of a frame nobody owns.
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let throws = assemble(vec![Op::ThrowConstAssignment], ConstantPool::default());
        let returns = assemble(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 3,
                },
                Op::Return { src: Register(0) },
            ],
            ConstantPool::default(),
        );

        assert!(interpreter.run(&throws).is_err());
        assert_eq!(interpreter.depth(), 0);
        assert_eq!(interpreter.run(&returns), Ok(Value::from_i32(3)));
        assert_eq!(interpreter.depth(), 0);
    }

    #[test]
    fn a_frame_does_not_see_what_the_last_one_left_behind() {
        // Reading a register lowering has not written to is undefined behaviour in the bytecode and
        // not in Rust, so the answer has to be `undefined` rather than whatever was there.
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let writes = assemble(
            vec![
                Op::LoadInt {
                    dst: Register(5),
                    value: 99,
                },
                Op::Return { src: Register(5) },
            ],
            ConstantPool::default(),
        );
        let reads = assemble(
            vec![Op::Return { src: Register(5) }],
            ConstantPool::default(),
        );

        assert_eq!(interpreter.run(&writes), Ok(Value::from_i32(99)));
        assert_eq!(interpreter.run(&reads), Ok(Value::UNDEFINED));
    }

    #[test]
    fn a_string_constant_refuses_rather_than_pretending() {
        let mut constants = ConstantPool::default();
        let hello = constants.string("hello");
        assert_eq!(
            run_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: hello,
                    },
                    Op::Return { src: Register(0) },
                ],
                constants
            ),
            Err(RuntimeError::NoStrings)
        );
    }

    #[test]
    fn typeof_refuses_for_the_same_reason() {
        assert_eq!(
            run(vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                },
                Op::TypeOf {
                    dst: Register(1),
                    src: Register(0),
                },
                Op::Return { src: Register(1) },
            ]),
            Err(RuntimeError::NoStrings)
        );
    }

    // The binary operators as functions, so that a table driven test can name one without writing
    // out the struct literal. Each of these is the constructor and nothing else.
    fn add(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Add {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn sub(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Sub {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn mul(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Mul {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn div(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Div {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn rem(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Rem {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn pow(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Pow {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn bit_or(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::BitOr {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn bit_xor(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::BitXor {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn bit_and(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::BitAnd {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn shl(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Shl {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn shr(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Shr {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn unsigned_shr(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::UnsignedShr {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn less(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Less {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn less_equal(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::LessEqual {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn greater(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Greater {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn greater_equal(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::GreaterEqual {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
}
