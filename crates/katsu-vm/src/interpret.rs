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
//! Everything that happens inside a single frame: loads, moves, the arithmetic, comparison and
//! string operators, the unary operators, the dead zone checks, jumps, back edges and `return`. Two
//! of the five JavaScript primitive types are here in full, numbers and strings, and the three that
//! have one value each are here because they are immediates in the value encoding.
//!
//! Calls, closures, environments, globals and property access are not here. They need an object with
//! a shape and M0 has strings and nothing else on the heap, so they land in the next piece of work
//! rather than being faked here. Every one of them reaches the same arm at the bottom of the match
//! and produces an error that names the opcode, which is a refusal rather than a wrong answer.
//!
//! # The one assumption about the heap
//!
//! Every heap object in M0 is a string. Nothing else is allocated yet, so a pointer is a string and
//! a range check settles it. That assumption stops being true in M1, and rather than spread it over
//! twenty arms it lives in exactly one function, [`Interpreter::as_string`], which is where the shape
//! read goes when there is a shape to read.

// Same reason as in `number`. JavaScript equality on numbers is exact IEEE equality, so comparing
// two doubles with `==` is the specified behaviour and not an oversight.
#![allow(clippy::float_cmp)]

use std::cmp::Ordering as Sorting;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use katsu_gc::StringRef;
use katsu_ir::{Constant, FunctionBlueprint, Op, Register};

use crate::number::{exponentiate, from_string, shift_count, to_int32, to_string, to_uint32};
use crate::stack::{Stack, StackError};
use crate::{Isolate, Value};

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
    /// The heap had no room for an object.
    ///
    /// Not a JavaScript error, because a program cannot catch running out of memory under Node
    /// either: the process prints and dies. It is an error rather than an abort here because M0's
    /// heap is a bump allocator with no collector behind it, so a full heap today usually means the
    /// collector is not written yet rather than that the program is genuinely too large.
    #[error("out of memory")]
    OutOfMemory,
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

/// One thread's worth of execution: a heap, a stack, and the flag that can stop it.
///
/// One thread is one isolate is one stack, so the interpreter owns the isolate rather than borrowing
/// it. Handing them out separately would allow two stacks over one heap, which is the arrangement
/// spec 3 rules out and the reason the collector needs no read barriers.
#[derive(Debug)]
pub struct Interpreter {
    isolate: Isolate,
    stack: Stack,
    interrupt: Interrupt,
}

impl Interpreter {
    /// Create an interpreter with an empty heap and an empty stack.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Range`] if the stack's or the heap's address space cannot be
    /// reserved, which means the process is out of address space and nothing else is going to work
    /// either.
    pub fn new() -> Result<Interpreter, RuntimeError> {
        Ok(Interpreter {
            isolate: Isolate::new().map_err(|error| RuntimeError::Range(error.to_string()))?,
            stack: Stack::new()?,
            interrupt: Interrupt::default(),
        })
    }

    /// The isolate this interpreter runs in.
    #[must_use]
    pub const fn isolate(&self) -> &Isolate {
        &self.isolate
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

    /// Render a value as text, the way `console.log` prints it at the top level.
    ///
    /// A value on its own means nothing, because a string is an address into this interpreter's
    /// cage, so anything that wants to look at one has to come back here. That is the ownership
    /// rule from spec 3 and it is why this is a method rather than a function on [`Value`].
    ///
    /// A string with a lone surrogate in it has no UTF-8 spelling, and this replaces the surrogate
    /// rather than refusing, because the alternative is a print that fails. Node does the same.
    #[must_use]
    pub fn display(&self, value: Value) -> String {
        match self.as_string(value) {
            Some(string) => string.to_utf8_lossy(self.isolate.cage()).into_owned(),
            None => Self::primitive_text(value),
        }
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
        let constants = self.resolve(blueprint)?;
        self.stack.push(blueprint.frame_size, &[], 0, Register(0))?;
        let outcome = self.execute(blueprint, &constants);
        self.stack.pop();
        outcome
    }

    /// Turn a blueprint's constant pool into values, once, before the first instruction runs.
    ///
    /// The pool holds Rust strings because the pass that built it could not reach the atom table,
    /// which is the arrangement written down at the top of `katsu-ir`'s constant module. This is the
    /// other half of it: one walk that interns every string and boxes every number, so that a
    /// `load_const` is an index into a slice rather than a hash of a literal.
    ///
    /// Doing it per run is the shape, not the destination. It belongs at load time, once per
    /// blueprint per realm, and it moves there when there is a realm to load into. A program that
    /// runs one blueprint once, which is every program M0 can run, cannot tell the difference.
    ///
    /// String constants are interned rather than merely allocated, because a literal is exactly the
    /// kind of string a program mentions repeatedly and every duplicate saved is a duplicate the
    /// collector never has to walk.
    fn resolve(&mut self, blueprint: &FunctionBlueprint) -> Result<Vec<Value>, RuntimeError> {
        blueprint
            .constants
            .values()
            .iter()
            .map(|constant| match constant {
                Constant::Number(number) => Ok(Value::from_f64(*number)),
                Constant::String(text) => {
                    let atom = self.isolate.intern(text).ok_or(RuntimeError::OutOfMemory)?;
                    Ok(self.string_value(atom.as_string()))
                }
            })
            .collect()
    }

    /// The loop.
    ///
    /// Long because the instruction set is long, and one arm per opcode with no shared helper is
    /// the point rather than an accident. `match_same_arms` is off for the same reason: `load_this`
    /// and `load_undefined` happen to produce the same value today and stop doing so the moment a
    /// function can be called with a receiver, so merging them would be merging two different
    /// questions that currently have the same answer.
    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    fn execute(
        &mut self,
        blueprint: &FunctionBlueprint,
        constants: &[Value],
    ) -> Result<Value, RuntimeError> {
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
                    let value = *constants
                        .get(src.0 as usize)
                        .expect("verify checked every constant index against the pool");
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

                // `+` is two operators sharing one symbol, and which one it is depends on the values
                // rather than on the syntax. The specification says to run ToPrimitive on both sides
                // first and then check whether either result is a string, and in M0 every value is
                // already primitive, so the first step is nothing and the check is the whole rule.
                Op::Add { dst, lhs, rhs, .. } => {
                    let left = self.stack.get(lhs);
                    let right = self.stack.get(rhs);
                    // Two numbers first, and the test for it is two tag checks on values already in
                    // registers. Asking whether either side is a string first would be correct and
                    // would put a cage relative bounds check in front of every addition in every
                    // program, which measured at more than twice the cost of the addition itself.
                    let value = match (left.as_f64(), right.as_f64()) {
                        (Some(left), Some(right)) => Value::from_f64(left + right),
                        _ => self.add_slowly(left, right)?,
                    };
                    self.stack.set(dst, value);
                }
                Op::Sub { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack.set(dst, Value::from_f64(lhs - rhs));
                }
                Op::Mul { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack.set(dst, Value::from_f64(lhs * rhs));
                }
                Op::Div { dst, lhs, rhs, .. } => {
                    // No check for a zero divisor. JavaScript has no integer division, so this is
                    // IEEE division all the way down and `1 / 0` is `Infinity` rather than a fault.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack.set(dst, Value::from_f64(lhs / rhs));
                }
                Op::Rem { dst, lhs, rhs, .. } => {
                    // Rust's `%` on floats truncates towards zero and keeps the sign of the
                    // dividend, which is what the specification asks for. `-5 % 3` is `-2` in both
                    // languages, and it is `1` in Python, which is the trap this comment exists for.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack.set(dst, Value::from_f64(lhs % rhs));
                }
                Op::Pow { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack.set(dst, Value::from_f64(exponentiate(lhs, rhs)));
                }

                Op::BitOr { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack
                        .set(dst, Value::from_i32(to_int32(lhs) | to_int32(rhs)));
                }
                Op::BitXor { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack
                        .set(dst, Value::from_i32(to_int32(lhs) ^ to_int32(rhs)));
                }
                Op::BitAnd { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    self.stack
                        .set(dst, Value::from_i32(to_int32(lhs) & to_int32(rhs)));
                }
                Op::Shl { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    let shifted = to_int32(lhs).wrapping_shl(shift_count(rhs));
                    self.stack.set(dst, Value::from_i32(shifted));
                }
                Op::Shr { dst, lhs, rhs, .. } => {
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    let shifted = to_int32(lhs).wrapping_shr(shift_count(rhs));
                    self.stack.set(dst, Value::from_i32(shifted));
                }
                Op::UnsignedShr { dst, lhs, rhs, .. } => {
                    // The one shift whose result does not fit an `i32`. `-1 >>> 0` is four billion
                    // and change, so the answer goes back as a number and lets the value encoding
                    // decide how to hold it.
                    let (lhs, rhs) = self.two_numbers(lhs, rhs);
                    let shifted = to_uint32(lhs).wrapping_shr(shift_count(rhs));
                    self.stack.set(dst, Value::from_f64(f64::from(shifted)));
                }

                Op::Equal { dst, lhs, rhs, .. } => {
                    let equal = self.loose_equal(self.stack.get(lhs), self.stack.get(rhs));
                    self.stack.set(dst, Value::from_bool(equal));
                }
                Op::NotEqual { dst, lhs, rhs, .. } => {
                    let equal = self.loose_equal(self.stack.get(lhs), self.stack.get(rhs));
                    self.stack.set(dst, Value::from_bool(!equal));
                }
                Op::StrictEqual { dst, lhs, rhs, .. } => {
                    let equal = self.strict_equal(self.stack.get(lhs), self.stack.get(rhs));
                    self.stack.set(dst, Value::from_bool(equal));
                }
                Op::StrictNotEqual { dst, lhs, rhs, .. } => {
                    let equal = self.strict_equal(self.stack.get(lhs), self.stack.get(rhs));
                    self.stack.set(dst, Value::from_bool(!equal));
                }

                // All four are the same abstract comparison asked in a different order, which is how
                // the specification defines them and is why they share one helper. The helper answers
                // `None` when a NaN is involved, and the four arms differ in what they make of that:
                // `<` and `>` answer false, and `<=` and `>=` answer false as well, but by way of a
                // negation, so the `None` has to become true before it is negated. Writing each of
                // them as its own float comparison agrees on numbers and disagrees on strings, where
                // `"a" <= "b"` is not `!("a" > "b")` by accident but by definition.
                Op::Less { dst, lhs, rhs, .. } => {
                    let less = self.less_than(self.stack.get(lhs), self.stack.get(rhs));
                    self.stack.set(dst, Value::from_bool(less.unwrap_or(false)));
                }
                Op::Greater { dst, lhs, rhs, .. } => {
                    // The operands go in the other way round, which is what makes `>` a `<` with its
                    // arguments swapped rather than a comparison of its own.
                    let less = self.less_than(self.stack.get(rhs), self.stack.get(lhs));
                    self.stack.set(dst, Value::from_bool(less.unwrap_or(false)));
                }
                Op::LessEqual { dst, lhs, rhs, .. } => {
                    let less = self.less_than(self.stack.get(rhs), self.stack.get(lhs));
                    self.stack.set(dst, Value::from_bool(!less.unwrap_or(true)));
                }
                Op::GreaterEqual { dst, lhs, rhs, .. } => {
                    let less = self.less_than(self.stack.get(lhs), self.stack.get(rhs));
                    self.stack.set(dst, Value::from_bool(!less.unwrap_or(true)));
                }

                Op::Neg { dst, src, .. } => {
                    let number = self.number_at(src);
                    self.stack.set(dst, Value::from_f64(-number));
                }
                Op::BitNot { dst, src, .. } => {
                    let number = self.number_at(src);
                    self.stack.set(dst, Value::from_i32(!to_int32(number)));
                }
                Op::ToNumber { dst, src, .. } => {
                    let number = self.number_at(src);
                    self.stack.set(dst, Value::from_f64(number));
                }
                Op::Inc { dst, src, .. } => {
                    let number = self.number_at(src);
                    self.stack.set(dst, Value::from_f64(number + 1.0));
                }
                Op::Dec { dst, src, .. } => {
                    let number = self.number_at(src);
                    self.stack.set(dst, Value::from_f64(number - 1.0));
                }
                Op::Not { dst, src } => {
                    let truthy = self.truthy(self.stack.get(src));
                    self.stack.set(dst, Value::from_bool(!truthy));
                }
                Op::TypeOf { dst, src } => {
                    let value = self.type_of_value(self.stack.get(src))?;
                    self.stack.set(dst, value);
                }

                Op::Jump { target } => pc = target.0 as usize,
                Op::JumpIfTrue { cond, target } => {
                    if self.truthy(self.stack.get(cond)) {
                        pc = target.0 as usize;
                    }
                }
                Op::JumpIfFalse { cond, target } => {
                    if !self.truthy(self.stack.get(cond)) {
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

    /// The one place a value becomes a string reference.
    ///
    /// Every heap object in M0 is a string, so a pointer is a string and this is a range check
    /// against the cage. The claim is wrong the moment there is a second kind of object, which is
    /// M1, and this is where the shape read goes when that happens. One chokepoint rather than a
    /// dozen scattered casts is the entire reason the function exists, because a scattered version
    /// of this assumption would have to be found before it could be fixed.
    fn as_string(&self, value: Value) -> Option<StringRef> {
        StringRef::from_slot(value.to_slot(self.isolate.cage())?)
    }

    /// Whether a value is a string, which is the same question with the answer thrown away.
    fn is_string(&self, value: Value) -> bool {
        self.as_string(value).is_some()
    }

    /// Wrap a string reference back up as a value.
    fn string_value(&self, string: StringRef) -> Value {
        Value::from_slot(string.slot(), self.isolate.cage())
    }

    /// Intern some text and hand back the value for it.
    ///
    /// For the strings the runtime itself produces, which are a small fixed set that turns up over
    /// and over: the answers to `typeof` today, and every property name and every built in later.
    fn intern(&mut self, text: &str) -> Result<Value, RuntimeError> {
        let atom = self.isolate.intern(text).ok_or(RuntimeError::OutOfMemory)?;
        Ok(self.string_value(atom.as_string()))
    }

    /// The ECMAScript `ToBoolean` abstract operation.
    ///
    /// [`Value::to_boolean`] answers this for everything that is not on the heap and cannot answer it
    /// for a string, because the empty string is the one falsy heap value in the language and telling
    /// it apart from every other string needs the cage. So the string case is here and the rest is
    /// delegated.
    ///
    /// The test for which of the two it is looks at the tag and not at the heap, because this is
    /// what every conditional jump in every loop goes through and a loop counter is not a pointer.
    #[inline]
    fn truthy(&self, value: Value) -> bool {
        if value.is_pointer() {
            return self.heap_truthy(value);
        }
        value.to_boolean()
    }

    /// `ToBoolean` of a value on the heap, which in M0 means a string.
    ///
    /// Out of line rather than cold, because `if (name)` is ordinary code and not a fallback. It is
    /// only kept out of the dispatch loop so that the loop stays small.
    #[inline(never)]
    fn heap_truthy(&self, value: Value) -> bool {
        match self.as_string(value) {
            Some(string) => !string.is_empty(self.isolate.cage()),
            None => value.to_boolean(),
        }
    }

    /// The `typeof` operator.
    ///
    /// Same split as [`Interpreter::truthy`] and for the same reason. `typeof` on a pointer is
    /// `"string"` for everything M0 has and `"object"` for most of what M1 adds, and only the cage
    /// can tell which.
    fn type_of(&self, value: Value) -> &'static str {
        if self.is_string(value) {
            return "string";
        }
        value.type_of()
    }

    /// `typeof` as the string value the opcode stores.
    ///
    /// Interned rather than allocated, because there are eight possible answers in the whole
    /// language and a program that asks the question in a loop should not be building a new string
    /// every time round. Out of line so that the hash the intern costs is not code the loop carries.
    #[inline(never)]
    fn type_of_value(&mut self, value: Value) -> Result<Value, RuntimeError> {
        let name = self.type_of(value);
        self.intern(name)
    }

    /// The ECMAScript `ToString` abstract operation, as a string on the heap.
    ///
    /// A string is already one, and everything else in M0 is a primitive whose text is short and
    /// entirely ASCII, so it is built as Rust text and copied in once. That is one allocation more
    /// than the arrangement deserves and it is the simple version of the right answer: once there
    /// are ropes, in M1, a number's text goes straight into the rope rather than through a `String`
    /// on the way.
    fn coerce_to_string(&mut self, value: Value) -> Result<StringRef, RuntimeError> {
        if let Some(string) = self.as_string(value) {
            return Ok(string);
        }
        let text = Self::primitive_text(value);
        self.isolate
            .allocate_string(&text)
            .ok_or(RuntimeError::OutOfMemory)
    }

    /// The text of a value that is not a string, which in M0 is every value not on the heap.
    ///
    /// The hole is not reachable here, because a register holding it is checked by the dead zone
    /// check before anything can read it, and a register that was never written holds `undefined`.
    fn primitive_text(value: Value) -> String {
        if value.is_undefined() {
            return "undefined".to_owned();
        }
        if value.is_null() {
            return "null".to_owned();
        }
        if let Some(boolean) = value.as_bool() {
            return boolean.to_string();
        }
        if let Some(number) = value.as_f64() {
            return to_string(number);
        }
        // Reachable only if the heap grows a second kind of object without this growing an arm for
        // it, which is the M1 change this file's header describes.
        "[object Object]".to_owned()
    }

    /// `+` when the two tag checks in the dispatch loop did not both say number.
    ///
    /// Which means a string on one side or the other, or an operand that has to convert first. Out
    /// of line and marked cold so that the addition in the loop stays a load, a test and an `fadd`,
    /// and so that none of this ends up inlined into the middle of the dispatch switch where it
    /// would cost every other opcode instruction cache it has no use for.
    #[cold]
    #[inline(never)]
    fn add_slowly(&mut self, left: Value, right: Value) -> Result<Value, RuntimeError> {
        if self.is_string(left) || self.is_string(right) {
            return self.concatenate(left, right);
        }
        Ok(Value::from_f64(self.number(left) + self.number(right)))
    }

    /// `+` when at least one side is a string.
    fn concatenate(&mut self, left: Value, right: Value) -> Result<Value, RuntimeError> {
        // Both conversions happen before the join, because the join borrows the heap mutably, and
        // left before right, because the specification converts in that order and a program can see
        // which one ran once a conversion can call `toString`.
        let left = self.coerce_to_string(left)?;
        let right = self.coerce_to_string(right)?;
        let joined = StringRef::concat(self.isolate.heap_mut(), left, right)
            .ok_or(RuntimeError::OutOfMemory)?;
        Ok(self.string_value(joined))
    }

    /// `ToNumber` of a value.
    ///
    /// Total, because every one of the five types M0 has converts to a number and none of the
    /// conversions can fail. It grows a `Result` in M1, when `ToPrimitive` on an object can call
    /// `valueOf`, which can throw. Giving it one now would mean an error arm nothing can reach,
    /// which reads as though the case had been handled when it had only been anticipated.
    ///
    /// A number returns itself, which is the case fifteen opcodes hit every time they run, and it is
    /// two instructions. Everything else is out of line. That split is worth spelling out because it
    /// is not a micro optimization: this is called from most of the arithmetic arms of the dispatch
    /// loop, and the conversions below it allocate and read the heap, so leaving them inline put a
    /// string decoder in the middle of the switch and cost every arithmetic opcode about a third of
    /// its running time, exponentiation included, which does not convert anything.
    #[inline]
    fn number(&self, value: Value) -> f64 {
        match value.as_f64() {
            Some(number) => number,
            None => self.number_slowly(value),
        }
    }

    /// `ToNumber` of a value that is not already a number.
    #[cold]
    #[inline(never)]
    fn number_slowly(&self, value: Value) -> f64 {
        if let Some(string) = self.as_string(value) {
            // A string that is not valid UTF-16 has a lone surrogate in it, and a lone surrogate is
            // neither whitespace nor a digit, so the answer is NaN without looking any further.
            return match string.to_utf8(self.isolate.cage()) {
                Ok(text) => from_string(&text),
                Err(_) => f64::NAN,
            };
        }
        if value.is_undefined() {
            return f64::NAN;
        }
        if value.is_null() {
            return 0.0;
        }
        if let Some(boolean) = value.as_bool() {
            return if boolean { 1.0 } else { 0.0 };
        }
        // Same argument as in `primitive_text`: unreachable until the heap holds something that is
        // not a string, and an honest NaN rather than a panic if it ever does.
        f64::NAN
    }

    /// `ToNumber` of one register.
    fn number_at(&self, register: Register) -> f64 {
        self.number(self.stack.get(register))
    }

    /// `ToNumber` of both operands of a binary operator, left to right.
    ///
    /// The order matters and is not an accident. Once these can call `valueOf`, the left operand's
    /// conversion runs first and can throw or have a side effect, and a program can see which one
    /// happened.
    fn two_numbers(&self, lhs: Register, rhs: Register) -> (f64, f64) {
        let lhs = self.number_at(lhs);
        let rhs = self.number_at(rhs);
        (lhs, rhs)
    }

    /// `===`, which is a type check and then a value check.
    ///
    /// Numbers compare as numbers rather than as bit patterns, because `1` and `1.0` are the same
    /// JavaScript number held two different ways, and because `NaN === NaN` is false while the two
    /// have identical bits. Strings compare by their contents rather than by their address, because
    /// two strings with the same code units are the same string in this language whether or not they
    /// were interned into the same object.
    #[inline]
    fn strict_equal(&self, left: Value, right: Value) -> bool {
        if let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) {
            return left == right;
        }
        self.strict_equal_slowly(left, right)
    }

    /// `===` when at least one side is not a number.
    #[cold]
    #[inline(never)]
    fn strict_equal_slowly(&self, left: Value, right: Value) -> bool {
        if let (Some(left), Some(right)) = (self.as_string(left), self.as_string(right)) {
            return left.equals(self.isolate.cage(), right);
        }
        // Everything left is an immediate, so identical bits and identical values are the same
        // question. A string against something that is not a string lands here too and is correctly
        // false, because a pointer never has the same bits as an immediate.
        left == right
    }

    /// `==`, written the way the specification writes it, as a conversion and then the same question.
    ///
    /// Recursive rather than a table, because recursion is what the standard's `IsLooselyEqual` is,
    /// and because the table version has a well known failure mode where one cell quietly disagrees
    /// with the one that should mirror it.
    ///
    /// Two numbers short circuit to the same answer `===` gives, so the common case does not pay for
    /// the recursion, and the rest of it stays out of the dispatch loop.
    #[inline]
    fn loose_equal(&self, left: Value, right: Value) -> bool {
        if let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) {
            return left == right;
        }
        self.loose_equal_slowly(left, right)
    }

    /// `==` when at least one side is not a number.
    #[cold]
    #[inline(never)]
    fn loose_equal_slowly(&self, left: Value, right: Value) -> bool {
        // Checked before anything else, because `null == undefined` is true and neither of them is
        // equal to anything else at all, not to zero, not to false, and not to the empty string.
        if left.is_nullish() || right.is_nullish() {
            return left.is_nullish() && right.is_nullish();
        }
        // Two of a kind is the strict question, which covers both numbers, both strings and both
        // booleans.
        if left.is_number() && right.is_number()
            || left.is_bool() && right.is_bool()
            || self.is_string(left) && self.is_string(right)
        {
            return self.strict_equal(left, right);
        }
        // A boolean converts to a number and the question is asked again, rather than being compared
        // against the other side directly. That extra step is why `"1" == true` is true: the boolean
        // becomes the number one, and then the string is converted as well.
        if let Some(boolean) = left.as_bool() {
            return self.loose_equal(Value::from_i32(i32::from(boolean)), right);
        }
        if let Some(boolean) = right.as_bool() {
            return self.loose_equal(left, Value::from_i32(i32::from(boolean)));
        }
        // The only pair left in M0 is a number against a string, and the string is the side that
        // converts, which is why `"0x10" == 16` is true.
        self.number(left) == self.number(right)
    }

    /// The ECMAScript abstract relational comparison, asking whether `left` is less than `right`.
    ///
    /// `None` is the specification's `undefined` result, which happens when a NaN turns up on either
    /// side and is the reason `a < b` and `!(a >= b)` are different questions.
    ///
    /// Two strings compare by code unit and everything else compares as numbers. Code unit order is
    /// not code point order and it is not any order a human would call alphabetical: `"Z" < "a"` is
    /// true because the capitals come first in the table, and `"ab" < "b"` is true because the
    /// comparison stops at the first unit that differs.
    ///
    /// The specification also carries a flag saying which operand to convert first, because `<=`
    /// converts its right operand before its left. Nothing in M0 can tell the difference, since no
    /// conversion here can run code or throw, so that flag arrives with `valueOf` rather than now.
    ///
    /// Two numbers are the case every loop condition in every program is, so they are tested for
    /// first and answered without touching the heap, for the same reason the addition above is.
    fn less_than(&self, left: Value, right: Value) -> Option<bool> {
        match (left.as_f64(), right.as_f64()) {
            (Some(left), Some(right)) => compare_numbers(left, right),
            _ => self.less_than_slowly(left, right),
        }
    }

    /// The relational comparison when at least one side is not already a number.
    #[cold]
    #[inline(never)]
    fn less_than_slowly(&self, left: Value, right: Value) -> Option<bool> {
        if let (Some(left), Some(right)) = (self.as_string(left), self.as_string(right)) {
            return Some(left.compare(self.isolate.cage(), right) == Sorting::Less);
        }
        compare_numbers(self.number(left), self.number(right))
    }
}

/// Whether one number is less than another, with the specification's `undefined` for a NaN.
fn compare_numbers(left: f64, right: f64) -> Option<bool> {
    if left.is_nan() || right.is_nan() {
        return None;
    }
    Some(left < right)
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
        number_with(code, ConstantPool::default())
    }

    #[track_caller]
    fn number_with(code: Vec<Op>, constants: ConstantPool) -> f64 {
        run_with(code, constants)
            .expect("should not throw")
            .as_f64()
            .expect("should return a number")
    }

    /// The boolean a sequence returned.
    #[track_caller]
    fn boolean(code: Vec<Op>) -> bool {
        boolean_with(code, ConstantPool::default())
    }

    #[track_caller]
    fn boolean_with(code: Vec<Op>, constants: ConstantPool) -> bool {
        run_with(code, constants)
            .expect("should not throw")
            .as_bool()
            .expect("should return a boolean")
    }

    /// The text of the string a sequence returned.
    ///
    /// The interpreter has to outlive the value, because a string value is an address in that
    /// interpreter's cage and means nothing without it. That is the ownership rule from spec 3
    /// showing up in a test helper, and it is the reason this cannot be written as a function that
    /// returns a `Value` and reads it afterwards.
    #[track_caller]
    fn text(code: Vec<Op>, constants: ConstantPool) -> String {
        let blueprint = assemble(code, constants);
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let value = interpreter.run(&blueprint).expect("should not throw");
        let string = interpreter
            .as_string(value)
            .expect("should return a string");
        string
            .to_utf8(interpreter.isolate.cage())
            .expect("should be well formed text")
            .into_owned()
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
    fn a_string_constant_loads_as_the_string_it_holds() {
        let mut constants = ConstantPool::default();
        let hello = constants.string("hello");
        assert_eq!(
            text(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: hello,
                    },
                    Op::Return { src: Register(0) },
                ],
                constants
            ),
            "hello"
        );
    }

    #[test]
    fn the_same_string_constant_twice_is_one_object_on_the_heap() {
        // The pool deduplicates before this and interning deduplicates here, and the point of doing
        // it at the second place too is that two different functions mentioning the same literal
        // still reach one object. Checked through the heap cursor, because the addresses being equal
        // is exactly the claim.
        let mut constants = ConstantPool::default();
        let first = constants.string("length");
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let blueprint = assemble(
            vec![
                Op::LoadConst {
                    dst: Register(0),
                    src: first,
                },
                Op::Return { src: Register(0) },
            ],
            constants,
        );
        let one = interpreter.run(&blueprint).expect("should not throw");
        let after_first = interpreter.isolate.heap_used();
        let two = interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(one, two);
        assert_eq!(interpreter.isolate.heap_used(), after_first);
    }

    #[test]
    fn adding_a_string_to_anything_concatenates_and_does_not_add() {
        // Every expected string below came from running the same expression under Node 24.18.0.
        let mut constants = ConstantPool::default();
        let empty = constants.string("");
        let prefix = constants.string("n=");
        let fraction = constants.number(1.5);
        let big = constants.number(1e21);

        let join = |left: Op, right: Op| {
            vec![
                left,
                right,
                Op::Add {
                    dst: Register(2),
                    lhs: Register(0),
                    rhs: Register(1),
                    cache: IC,
                },
                Op::Return { src: Register(2) },
            ]
        };
        let load = |dst: u16, src| Op::LoadConst {
            dst: Register(dst),
            src,
        };
        let int = |dst: u16, value| Op::LoadInt {
            dst: Register(dst),
            value,
        };

        let cases = [
            (join(load(0, prefix), int(1, 1)), "n=1"),
            // The other way round, which is the same operator reaching the same branch from the
            // other side.
            (join(int(0, 1), load(1, prefix)), "1n="),
            (join(load(0, empty), load(1, fraction)), "1.5"),
            // Through our own `Number::toString` rather than Rust's, which would print this one as
            // a one followed by twenty one zeros.
            (join(load(0, empty), load(1, big)), "1e+21"),
            (
                join(
                    load(0, empty),
                    Op::LoadBool {
                        dst: Register(1),
                        value: true,
                    },
                ),
                "true",
            ),
            (
                join(load(0, empty), Op::LoadNull { dst: Register(1) }),
                "null",
            ),
            (
                join(load(0, empty), Op::LoadUndefined { dst: Register(1) }),
                "undefined",
            ),
        ];
        for (code, expected) in cases {
            assert_eq!(text(code, constants.clone()), expected);
        }
    }

    #[test]
    fn concatenation_associates_the_way_the_source_was_written() {
        // `"a" + 1 + 2` is `"a12"` and `1 + 2 + "a"` is `"3a"`, which is the same three values and
        // two different answers, and is the clearest statement of what the operator actually does.
        let mut constants = ConstantPool::default();
        let a = constants.string("a");
        let chain = |first: Op, second: Op, third: Op| {
            vec![
                first,
                second,
                third,
                Op::Add {
                    dst: Register(3),
                    lhs: Register(0),
                    rhs: Register(1),
                    cache: IC,
                },
                Op::Add {
                    dst: Register(3),
                    lhs: Register(3),
                    rhs: Register(2),
                    cache: IC,
                },
                Op::Return { src: Register(3) },
            ]
        };
        let load = |dst: u16| Op::LoadConst {
            dst: Register(dst),
            src: a,
        };
        let int = |dst: u16, value| Op::LoadInt {
            dst: Register(dst),
            value,
        };

        assert_eq!(
            text(chain(load(0), int(1, 1), int(2, 2)), constants.clone()),
            "a12"
        );
        assert_eq!(text(chain(int(0, 1), int(1, 2), load(2)), constants), "3a");
    }

    #[test]
    fn typeof_names_the_type_and_the_answer_is_a_string() {
        let mut constants = ConstantPool::default();
        let hello = constants.string("hello");
        let ask = |load: Op| {
            vec![
                load,
                Op::TypeOf {
                    dst: Register(1),
                    src: Register(0),
                },
                Op::Return { src: Register(1) },
            ]
        };
        let cases = [
            (
                ask(Op::LoadConst {
                    dst: Register(0),
                    src: hello,
                }),
                "string",
            ),
            (
                ask(Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                }),
                "number",
            ),
            (
                ask(Op::LoadBool {
                    dst: Register(0),
                    value: true,
                }),
                "boolean",
            ),
            (ask(Op::LoadUndefined { dst: Register(0) }), "undefined"),
            // The bug from 1995 that is in the standard.
            (ask(Op::LoadNull { dst: Register(0) }), "object"),
        ];
        for (code, expected) in cases {
            assert_eq!(text(code, constants.clone()), expected);
        }
    }

    #[test]
    fn strings_are_equal_by_their_contents_and_not_by_their_address() {
        // The first pair are two separately built strings with the same text, which is the case an
        // address comparison would get wrong. They are built by concatenation rather than loaded,
        // because two equal literals share a pool entry and would prove nothing.
        let mut constants = ConstantPool::default();
        let a = constants.string("a");
        let b = constants.string("b");
        let code = vec![
            Op::LoadConst {
                dst: Register(0),
                src: a,
            },
            Op::LoadConst {
                dst: Register(1),
                src: b,
            },
            Op::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                cache: IC,
            },
            Op::Add {
                dst: Register(3),
                lhs: Register(0),
                rhs: Register(1),
                cache: IC,
            },
            Op::StrictEqual {
                dst: Register(4),
                lhs: Register(2),
                rhs: Register(3),
                cache: IC,
            },
            Op::Return { src: Register(4) },
        ];
        assert_eq!(run_with(code, constants), Ok(Value::TRUE));
    }

    #[test]
    fn a_string_is_never_strictly_equal_to_a_number_and_is_loosely_equal_to_one() {
        // `"1" === 1` is false and `"1" == 1` is true, and the second one is the reason the first
        // one exists.
        let mut constants = ConstantPool::default();
        let one = constants.string("1");
        let compare = |op: fn(Register, Register, Register, CacheIndex) -> Op| {
            vec![
                Op::LoadConst {
                    dst: Register(0),
                    src: one,
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 1,
                },
                op(Register(2), Register(0), Register(1), IC),
                Op::Return { src: Register(2) },
            ]
        };
        assert_eq!(
            run_with(compare(strict_equal), constants.clone()),
            Ok(Value::FALSE)
        );
        assert_eq!(run_with(compare(equal), constants), Ok(Value::TRUE));
    }

    #[test]
    fn loose_equality_converts_the_string_and_converts_a_boolean_first() {
        // All four expectations came from Node. The last two are the ones that catch a table written
        // by hand: a boolean is not compared against anything directly, it becomes a number and the
        // question is asked again, which is why `"01" == true` is true.
        let mut constants = ConstantPool::default();
        let cases = [
            (constants.string(""), 0, true),
            (constants.string(" "), 0, true),
            // A hexadecimal string converts through the full numeric grammar, not through `parse`.
            (constants.string("0x10"), 16, true),
            (constants.string("2"), 1, false),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                boolean_with(
                    vec![
                        Op::LoadConst {
                            dst: Register(0),
                            src: left,
                        },
                        Op::LoadInt {
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
                    ],
                    constants.clone()
                ),
                expected
            );
        }

        let boolean_cases = [
            (constants.string("1"), true, true),
            (constants.string("01"), true, true),
            (constants.string(""), false, true),
            (constants.string("a"), true, false),
        ];
        for (left, right, expected) in boolean_cases {
            assert_eq!(
                boolean_with(
                    vec![
                        Op::LoadConst {
                            dst: Register(0),
                            src: left,
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
                    ],
                    constants.clone()
                ),
                expected
            );
        }
    }

    #[test]
    fn null_is_not_loosely_equal_to_the_empty_string() {
        // The nullish check comes first for exactly this: the empty string is falsy, `null` is
        // falsy, and they are still not equal.
        let mut constants = ConstantPool::default();
        let empty = constants.string("");
        assert!(!boolean_with(
            vec![
                Op::LoadNull { dst: Register(0) },
                Op::LoadConst {
                    dst: Register(1),
                    src: empty,
                },
                Op::Equal {
                    dst: Register(2),
                    lhs: Register(0),
                    rhs: Register(1),
                    cache: IC,
                },
                Op::Return { src: Register(2) },
            ],
            constants
        ));
    }

    #[test]
    fn two_strings_compare_by_code_unit_and_a_string_against_a_number_does_not() {
        // `"10" < "9"` is true and `"10" < 9` is false, which is one comparison operator answering
        // two different questions depending on what is on the other side.
        let mut constants = ConstantPool::default();
        let ten = constants.string("10");
        let nine = constants.string("9");
        let ab = constants.string("ab");
        let b = constants.string("b");
        let capital_z = constants.string("Z");
        let a = constants.string("a");

        let both = |left, right, op: fn(Register, Register, Register, CacheIndex) -> Op| {
            vec![
                Op::LoadConst {
                    dst: Register(0),
                    src: left,
                },
                Op::LoadConst {
                    dst: Register(1),
                    src: right,
                },
                op(Register(2), Register(0), Register(1), IC),
                Op::Return { src: Register(2) },
            ]
        };

        assert!(boolean_with(both(ten, nine, less), constants.clone()));
        // Shorter first when the shorter one is a prefix, and when it is not, the first unit that
        // differs decides.
        assert!(boolean_with(both(ab, b, less), constants.clone()));
        // Capitals sort before lower case, because that is where they are in the table.
        assert!(boolean_with(both(capital_z, a, less), constants.clone()));
        assert!(boolean_with(both(a, a, less_equal), constants.clone()));
        assert!(boolean_with(both(a, a, greater_equal), constants.clone()));
        assert!(!boolean_with(both(a, a, less), constants.clone()));

        // And the same two strings against a number, which converts both sides.
        assert!(!boolean_with(
            vec![
                Op::LoadConst {
                    dst: Register(0),
                    src: ten,
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 9,
                },
                Op::Less {
                    dst: Register(2),
                    lhs: Register(0),
                    rhs: Register(1),
                    cache: IC,
                },
                Op::Return { src: Register(2) },
            ],
            constants
        ));
    }

    #[test]
    fn a_string_that_is_not_a_number_makes_every_comparison_false() {
        // The NaN rule reached through a string rather than through a literal NaN, which is where a
        // hand written `<=` written as `!(>)` gets it wrong.
        let mut constants = ConstantPool::default();
        let word = constants.string("a");
        for build in [less, less_equal, greater, greater_equal] {
            let op = build(Register(2), Register(0), Register(1), IC);
            assert!(
                !boolean_with(
                    vec![
                        Op::LoadConst {
                            dst: Register(0),
                            src: word,
                        },
                        Op::LoadInt {
                            dst: Register(1),
                            value: 1,
                        },
                        op,
                        Op::Return { src: Register(2) },
                    ],
                    constants.clone()
                ),
                "{op} said true about a string that is not a number"
            );
        }
    }

    #[test]
    fn the_empty_string_is_the_one_falsy_thing_on_the_heap() {
        let mut constants = ConstantPool::default();
        let cases = [
            (constants.string(""), true),
            (constants.string("a"), false),
            // Non empty and therefore truthy, even though converting it to a number gives zero.
            (constants.string("0"), false),
        ];
        for (string, expected) in cases {
            assert_eq!(
                boolean_with(
                    vec![
                        Op::LoadConst {
                            dst: Register(0),
                            src: string,
                        },
                        Op::Not {
                            dst: Register(1),
                            src: Register(0),
                        },
                        Op::Return { src: Register(1) },
                    ],
                    constants.clone()
                ),
                expected
            );
        }
    }

    #[test]
    fn an_operator_that_is_not_plus_converts_a_string_to_a_number() {
        // `+` is the only operator that treats a string as a string, and this is the other side of
        // that: everything else runs ToNumber first, which is why `"3" - 1` is `2` and `"a" - 1` is
        // NaN rather than an error.
        let mut constants = ConstantPool::default();
        let three = constants.string("3");
        let word = constants.string("a");
        let arithmetic = |left, op: fn(Register, Register, Register, CacheIndex) -> Op| {
            vec![
                Op::LoadConst {
                    dst: Register(0),
                    src: left,
                },
                Op::LoadInt {
                    dst: Register(1),
                    value: 1,
                },
                op(Register(2), Register(0), Register(1), IC),
                Op::Return { src: Register(2) },
            ]
        };

        assert_eq!(number_with(arithmetic(three, sub), constants.clone()), 2.0);
        assert!(number_with(arithmetic(word, sub), constants.clone()).is_nan());
        // The unary ones as well, since they take the same path.
        assert_eq!(
            number_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: three,
                    },
                    Op::Neg {
                        dst: Register(1),
                        src: Register(0),
                        cache: IC,
                    },
                    Op::Return { src: Register(1) },
                ],
                constants.clone()
            ),
            -3.0
        );
        assert_eq!(
            number_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: three,
                    },
                    Op::LoadInt {
                        dst: Register(1),
                        value: 0,
                    },
                    Op::BitOr {
                        dst: Register(2),
                        lhs: Register(0),
                        rhs: Register(1),
                        cache: IC,
                    },
                    Op::Return { src: Register(2) },
                ],
                constants
            ),
            3.0
        );
    }

    #[test]
    fn a_string_built_in_a_loop_comes_out_in_order() {
        // The one program in this file that uses the heap the way a real one would, and the reason
        // the comment on `StringRef::concat` says ropes are M1's job: this is quadratic today.
        let mut constants = ConstantPool::default();
        let start = constants.string("");
        let piece = constants.string("ab");
        assert_eq!(
            text(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: start,
                    },
                    Op::LoadConst {
                        dst: Register(1),
                        src: piece,
                    },
                    Op::LoadInt {
                        dst: Register(2),
                        value: 0,
                    },
                    Op::LoadInt {
                        dst: Register(3),
                        value: 3,
                    },
                    Op::Less {
                        dst: Register(4),
                        lhs: Register(2),
                        rhs: Register(3),
                        cache: IC,
                    },
                    Op::JumpIfFalse {
                        cond: Register(4),
                        target: CodeOffset(9),
                    },
                    Op::Add {
                        dst: Register(0),
                        lhs: Register(0),
                        rhs: Register(1),
                        cache: IC,
                    },
                    Op::Inc {
                        dst: Register(2),
                        src: Register(2),
                        cache: IC,
                    },
                    Op::LoopBackEdge {
                        target: CodeOffset(4),
                        profile: IC,
                    },
                    Op::Return { src: Register(0) },
                ],
                constants
            ),
            "ababab"
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
    fn equal(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::Equal {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
    fn strict_equal(dst: Register, lhs: Register, rhs: Register, cache: CacheIndex) -> Op {
        Op::StrictEqual {
            dst,
            lhs,
            rhs,
            cache,
        }
    }
}
