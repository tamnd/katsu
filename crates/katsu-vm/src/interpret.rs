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
//! Calls are here too, which means closures, environments and captured variables are here. A
//! function value is a closure, a call pushes a frame and moves the three locals above the loop, and
//! a return pops one and puts them back. That is enough for recursion, for a function that reads a
//! variable from the scope it was written in, and for a function that outlives that scope.
//!
//! Globals are here, and so is the other kind of call: a value can be a function whose body is Rust
//! rather than bytecode, and calling one leaves the loop, runs the Rust and comes back with a value.
//! That is the whole of how a program reaches anything the runtime implements for it, and it is why
//! a global load in front of a call is now a program that runs rather than a program that stops.
//!
//! Objects are here, with shapes behind them, which means a literal, a named property read, a named
//! property write and a method call all run. A key computed at run time does not: it reaches the arm
//! at the bottom of the match and produces an error that names the opcode, which is a refusal rather
//! than a wrong answer. Prototypes are not here either, so a property nothing wrote is `undefined`
//! rather than something inherited.
//!
//! Exceptions are here. `throw` is one instruction, a `try` is no instructions at all, and where a
//! throw goes is decided by [`Interpreter::handle`] reading the handler table the frontend wrote.
//! The search crosses frames, so a throw deep inside a call finds a handler above it, and the frames
//! in between are popped on the way. There is no `Error` constructor to be an instance of, so a
//! caught engine error is an object with `name` and `message` rather than an instance of `Error`.
//!
//! # The one assumption about the heap
//!
//! There are several kinds of heap object now, strings, closures, contexts, natives, objects, shapes
//! and overflow property blocks, and the first word says which. A pointer is not self describing, so
//! the functions that turn a value into an object, [`Interpreter::as_string`],
//! [`Interpreter::as_closure`], [`Interpreter::as_native`] and [`Interpreter::as_object`], read that
//! word before they believe anything. Everywhere else in the loop goes through one of them, which is
//! what keeps the assumption in four places instead of twenty.

// Same reason as in `number`. JavaScript equality on numbers is exact IEEE equality, so comparing
// two doubles with `==` is the specified behaviour and not an oversight.
#![allow(clippy::float_cmp)]

use std::cmp::Ordering as Sorting;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use katsu_gc::{
    AccessorPairRef, Attributes, ClosureRef, ContextRef, HeapKind, NativeRef, ObjectRef, StringRef,
};
use katsu_ir::{AccessorHalf, Constant, FunctionBlueprint, Op, Register};
use smallvec::SmallVec;

use crate::inspect;
use crate::number::{exponentiate, from_string, shift_count, to_int32, to_string, to_uint32};
use crate::stack::{Invocation, Stack, StackError};
use crate::unit::{Resolved, Unit};
use crate::{Isolate, Value};

/// Why execution stopped somewhere other than a `return`.
///
/// Some of these are exceptions and some of them are not, and the difference is not a flag on the
/// variant. It is the list the unwinder checks before it starts looking for a handler:
/// running out of memory, being asked to stop, and reaching an opcode nobody has written yet are
/// things that happened to a program rather than things a program did, and Node cannot catch its
/// equivalents either.
///
/// The three that carry a message stay a message until something catches one. Turning one into an
/// object costs an allocation and a shape transition, and almost none of them are ever caught, so
/// that is paid for at the `catch` rather than at the throw.
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
    /// A value the program threw, on its way up the stack.
    ///
    /// The only variant holding a value rather than a message, because `throw` takes an expression
    /// and an expression can produce anything. It never leaves [`Interpreter::run`]: a value is an
    /// address in one interpreter's heap and means nothing outside it, so the top of the stack
    /// turns one into [`RuntimeError::Uncaught`] while it still holds both. The message here is the
    /// same sentence whatever the value is, and it is written because the type needs one rather
    /// than because anything reads it.
    #[error("an exception reached the top of the stack")]
    Thrown(Value),
    /// A thrown value nothing caught, printed the way `console.log` would have printed it.
    #[error("{0}")]
    Uncaught(String),
    /// An opcode that lowering can emit and the interpreter cannot run yet.
    ///
    /// Not a panic, because the point of it is that a program using a construct we have not finished
    /// gets a clear refusal naming the opcode rather than a wrong answer or a crash.
    #[error("{0} is not implemented yet")]
    NotImplemented(Op),
    /// A builtin that exists but has not been finished, refusing by name.
    ///
    /// Separate from [`RuntimeError::NotImplemented`] because that one names an opcode and this one
    /// names a piece of the standard library, and because a native has no opcode to point at. Both
    /// mean the same thing to everybody above: this is our gap and not the program's mistake.
    ///
    /// It exists so that a half built builtin can say so. The alternative is leaving the method off
    /// the object, which makes a missing feature arrive as `JSON.parse is not a function`, and that
    /// is an ordinary JavaScript error that a program will feature detect around and a reader will
    /// take for a bug in their own code.
    #[error("{0}")]
    Unsupported(String),
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

/// Where an unwind ended up: which function is running now, and where in it.
///
/// Two numbers rather than the four variables the loop keeps, because the other two are derived
/// from the function index and looking them up is what the loop does after a call as well. This is
/// what [`Interpreter::handle`] hands back, and it is a struct so that a caller cannot get the pair
/// the wrong way round.
#[derive(Clone, Copy, Debug)]
struct Landing {
    function: u32,
    pc: usize,
}

/// What a property read found, which is not always a value.
///
/// A read of an accessor cannot answer with a value, because producing the value means calling a
/// function, and calling a function is something only the loop can do. So the lookup reports what it
/// found and the opcode decides what to do about it, which keeps every frame push in the one place
/// that already knows how to push a frame.
///
/// Two variants and not three: a property with no getter has already been turned into `undefined` by
/// the time this is built, because that decision needs nothing the caller knows and answering it
/// here means the opcode has one case fewer to handle.
///
/// The receiver is not in here. It would be the same value the caller passed in, because a getter is
/// called on where the read started and not on where the property was found, so carrying it back
/// would hand the caller something it is already holding.
#[derive(Clone, Copy, Debug)]
enum Found {
    /// A value, which is what every read finds until a program defines an accessor.
    Value(Value),
    /// A getter to call, on the object the read started from.
    Getter(Value),
}

/// What a store did, which like a read is not always the obvious thing.
///
/// The refusals and the setter are kept apart from each other because they end in different places.
/// A refusal is silence in sloppy mode and a `TypeError` in strict mode, and that decision belongs to
/// the opcode because it is the opcode that knows which mode the code is in. A setter is a call, and
/// that also belongs to the opcode, because the loop is the only thing that can push a frame.
///
/// The two refusals are separate because node says two different things, which was measured rather
/// than assumed. A read only data property gives `Cannot assign to read only property 'x' of object
/// '#<Object>'` and an accessor with no setter gives `Cannot set property x of #<Object> which has
/// only a getter`, and neither sentence is a rewording of the other.
#[derive(Clone, Copy, Debug)]
enum Wrote {
    /// The value is in the object.
    Yes,
    /// The property is a data property that will not take a write.
    Refused,
    /// The property is an accessor with no setter, so there is nothing to write through.
    NoSetter,
    /// Nothing was written yet, because the write is a call to this function.
    Setter(Value),
}

/// A call the interpreter makes on a program's behalf rather than because one was written.
///
/// Accessors are the only source of these so far. Reading a property that turns out to be a getter
/// is a call, and so is writing one that turns out to be a setter, and neither has an opcode of its
/// own to carry the operands a call needs. Gathering them into a struct is so that a caller cannot
/// swap the target and the receiver, which are both `Value` and would compile either way round.
#[derive(Clone, Copy, Debug)]
struct Call {
    /// The function to call.
    target: Value,
    /// What `this` is inside it.
    receiver: Value,
    /// The caller's register the arguments start at, ignored when nothing is passed.
    first: Register,
    /// How many arguments start there.
    passed: u16,
    /// Where the answer goes, or [`Register::NOWHERE`] when nothing wants it.
    return_to: Register,
    /// The instruction in the caller to come back to.
    return_pc: u32,
}

/// What making one of those calls did.
///
/// A JavaScript function suspends the caller and the loop has to pick up the callee, and a function
/// written in Rust has already finished by the time this is built. Both are calls a program cannot
/// tell apart, and both have to be possible here, because a getter can be either.
enum Entered<'a> {
    /// A frame was pushed and the loop has to switch to the function named here.
    Frame {
        /// Which function in the loaded unit was entered.
        index: u32,
        /// Its blueprint, which the loop keeps in a local.
        callee: &'a FunctionBlueprint,
    },
    /// The callee was written in Rust, so it has already run and this is what it answered.
    Answered(Value),
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
///
/// The isolate is behind a pointer and the stack is not. An isolate is two hundred and eighty bytes
/// of heap bookkeeping that the dispatch loop only touches when a string is involved, and the stack
/// is what every single instruction reads and writes. Holding the isolate inline makes the
/// interpreter three hundred and forty four bytes instead of seventy two, and that measured slower
/// on both reference machines: a move, the cheapest instruction there is, went from 0.86 ns to 1.07
/// ns on a pinned 13900K and from 0.79 ns to 0.94 ns on an M4. Putting the stack first with
/// `repr(C)` recovered almost none of it, so it is the size of the thing the loop holds and not the
/// offsets inside it.
#[derive(Debug)]
pub struct Interpreter {
    isolate: Box<Isolate>,
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
            isolate: Box::new(
                Isolate::new().map_err(|error| RuntimeError::Range(error.to_string()))?,
            ),
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

    /// Bind a name in the global scope, creating it or replacing what was there.
    ///
    /// This is how anything gets into a program that the program did not write: the builtins, and
    /// whatever an embedder wants its scripts to be able to reach. The name is interned, so a later
    /// mention of the same text in a source file finds this binding.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the name cannot be interned.
    pub fn define_global(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        let key = self.isolate.intern(name).ok_or(RuntimeError::OutOfMemory)?;
        self.isolate.globals_mut().set(key.as_string(), value);
        Ok(())
    }

    /// What `name` is bound to in the global scope, or `None` if nothing is.
    ///
    /// Takes `&mut self` because looking a name up means interning it, and interning allocates. That
    /// is the price of names being addresses, and it is paid by the caller that asks a question in
    /// text rather than by every load in the dispatch loop.
    pub fn global(&mut self, name: &str) -> Option<Value> {
        let key = self.isolate.intern(name)?;
        self.isolate.globals().get(key.as_string())
    }

    /// Make a function value whose body is Rust, without binding it to anything.
    ///
    /// For a native that belongs somewhere other than the global scope, which from M0's point of
    /// view means a method on a host object.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap is full or the table is somehow full.
    pub fn native_function(
        &mut self,
        name: &str,
        call: crate::NativeFn,
    ) -> Result<Value, RuntimeError> {
        let ordinal = self
            .isolate
            .natives_mut()
            .add(name, call)
            .ok_or(RuntimeError::OutOfMemory)?;
        let native =
            NativeRef::new(self.isolate.heap_mut(), ordinal).ok_or(RuntimeError::OutOfMemory)?;
        Ok(Value::from_slot(native.slot(), self.isolate.cage()))
    }

    /// Make a function written in Rust and bind it in the global scope under the same name.
    ///
    /// # Errors
    ///
    /// As [`Interpreter::native_function`] and [`Interpreter::define_global`].
    pub fn define_native(&mut self, name: &str, call: crate::NativeFn) -> Result<(), RuntimeError> {
        let value = self.native_function(name, call)?;
        self.define_global(name, value)
    }

    /// Make an object with a fixed set of named properties on it.
    ///
    /// This is how the runtime installs the things a program expects to already be there, which in
    /// M0 is `console` and in M2 is most of the standard library. The names are interned, because a
    /// property name is exactly the kind of string a program mentions over and over and the lookup is
    /// a compare of two addresses once they are.
    ///
    /// The object is built with room for exactly the names given, so a host object costs one
    /// allocation and nothing on the side, and it is an ordinary object rather than a special kind
    /// of one. A program can add a property to `console` because a program can add a property to
    /// anything, which is what Node does too.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the names or the object.
    pub fn host_object(&mut self, entries: &[(&str, Value)]) -> Result<Value, RuntimeError> {
        self.host_object_with(entries, Attributes::DEFAULT)
    }

    /// The same, with a say in what the properties are allowed to do.
    ///
    /// [`Attributes::BUILTIN`] is the one every namespace object in the language is built with, and
    /// it is not a detail. `Math` and `JSON` hold nothing enumerable, so `Object.keys(JSON)` is empty
    /// and `console.log(Math)` prints an empty object rather than the whole standard library, and a
    /// `for in` over anything does not walk into them. `console` is the exception and keeps the
    /// default, because its methods really are enumerable own properties in Node.
    ///
    /// # Errors
    ///
    /// As [`Interpreter::host_object`].
    pub fn host_object_with(
        &mut self,
        entries: &[(&str, Value)],
        attributes: Attributes,
    ) -> Result<Value, RuntimeError> {
        let mut named = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            let atom = self.isolate.intern(name).ok_or(RuntimeError::OutOfMemory)?;
            named.push((atom.as_string(), value.to_bits()));
        }
        let object = self.new_object(named.len())?;
        for (name, value) in named {
            object
                .define(self.isolate.heap_mut(), name, value, attributes)
                .ok_or(RuntimeError::OutOfMemory)?;
        }
        Ok(Value::from_slot(object.slot(), self.isolate.cage()))
    }

    /// Build an empty object with room inside it for `properties` values.
    ///
    /// The count is a promise about what is about to be added rather than a limit: an object that
    /// gains more than it was built for grows, it just pays for a properties array on the side when
    /// it does.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the object or for the root
    /// shape, which is built on the first object this isolate ever makes.
    fn new_object(&mut self, properties: usize) -> Result<ObjectRef, RuntimeError> {
        let root = self.isolate.root_shape().ok_or(RuntimeError::OutOfMemory)?;
        let inline = u32::try_from(properties).map_err(|_| RuntimeError::OutOfMemory)?;
        ObjectRef::new(self.isolate.heap_mut(), root, inline).ok_or(RuntimeError::OutOfMemory)
    }

    /// Send everything this interpreter prints somewhere other than the process's own streams.
    ///
    /// Returns the sink that was there, so a caller can put it back. See [`Isolate::set_output`].
    pub fn set_output(&mut self, output: Box<dyn crate::Output>) -> Box<dyn crate::Output> {
        self.isolate.set_output(output)
    }

    /// Write to this interpreter's sink, which is what every printing builtin ends up calling.
    pub fn write_output(&mut self, stream: crate::Stream, text: &str) {
        self.isolate.write_output(stream, text);
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
        if let Some(string) = self.as_string(value) {
            return string.to_utf8_lossy(self.isolate.cage()).into_owned();
        }
        if let Some(closure) = self.as_closure(value) {
            return self.function_text(closure);
        }
        if let Some(native) = self.as_native(value) {
            return self.native_text(native);
        }
        if let Some(object) = self.as_object(value) {
            // A fresh cycle set per printed value, because the numbering restarts for each one.
            // `console.log(a, a)` on an object that holds itself prints `<ref *1>` twice rather
            // than numbering the second one two.
            return self.object_text(object, inspect::DEPTH, 0, &mut Cycles::default());
        }
        // Negative zero is the one number that inspection and `ToString` spell differently. The
        // specification says `ToString` of it is "0", and Node's console prints "-0", because a
        // console that cannot tell you which zero you have is hiding the thing you turned the
        // console on to see. So the case lives here, in the inspection path, and deliberately not
        // in `primitive_text`, which `coerce_to_string` calls and which has to keep saying "0" for
        // `'' + -0`. The differential harness found this on its first run against node.
        if let Some(number) = value.as_f64()
            && number == 0.0
            && number.is_sign_negative()
        {
            return "-0".to_owned();
        }
        Self::primitive_text(value)
    }

    /// `ToString` of a value, as Rust text.
    ///
    /// This is the language's conversion and not inspection, so an object is `[object Object]` and
    /// negative zero is `"0"`. [`Interpreter::display`] is the other one and the two differ on
    /// exactly those two cases on purpose.
    ///
    /// It goes through the same private `text_of` that `'' + x` goes through, rather than
    /// reimplementing the rules, because the one thing `String(x)` must never do is disagree with
    /// concatenation about what a value's text is.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Type`] for an object with no `toString` anywhere on its prototype
    /// chain, which is what `Object.create(null)` makes. Every other value has a text.
    pub fn to_text(&self, value: Value) -> Result<String, RuntimeError> {
        self.text_of(value)
    }

    /// Put text on the heap and hand back a value pointing at it.
    ///
    /// How a builtin written in Rust returns a string. Everything else it might want to return is a
    /// primitive that fits in a [`Value`] on its own, so this is the only allocation a builtin has to
    /// ask the interpreter for, and it has to ask, because a string is an address into this
    /// interpreter's cage and nowhere else.
    ///
    /// Not interned. An atom costs a hash and a lookup and earns them back when the same text is
    /// mentioned many times, which is true of property names and false of the answer a builtin
    /// computed for one call.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the text.
    pub fn new_string(&mut self, text: &str) -> Result<Value, RuntimeError> {
        let string = self
            .isolate
            .allocate_string(text)
            .ok_or(RuntimeError::OutOfMemory)?;
        Ok(Value::from_slot(string.slot(), self.isolate.cage()))
    }

    /// The text of a value that really is a string, and nothing else.
    ///
    /// `None` for a number, a boolean, an object or anything else, without converting. A builtin
    /// that has to treat `"1"` differently from `1` needs to ask this rather than asking for the
    /// text, and `JSON.stringify` is exactly that builtin: one of those two comes out quoted.
    #[must_use]
    pub fn as_text(&self, value: Value) -> Option<String> {
        self.as_string(value)
            .map(|string| string.to_utf8_lossy(self.isolate.cage()).into_owned())
    }

    /// What `value` inherits from: the prototype, `null` if it inherits from nothing, or `None` if
    /// `value` is not an object at all.
    ///
    /// The three answers are separate on purpose. `null` is a real prototype chain that ends, which
    /// is what `Object.create(null)` makes, and `None` is a question that does not apply, which is
    /// what a number is until there are wrapper prototypes to answer it with. Collapsing them would
    /// have `Object.getPrototypeOf(1)` quietly report `null` when the real answer is
    /// `Number.prototype`.
    #[must_use]
    pub fn prototype_of(&self, value: Value) -> Option<Value> {
        let object = self.as_object(value)?;
        Some(
            object
                .prototype(self.isolate.cage())
                .map_or(Value::NULL, |prototype| {
                    Value::from_slot(prototype.slot(), self.isolate.cage())
                }),
        )
    }

    /// `Object.prototype`, the object at the top of almost every chain in this realm.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for it, which at startup means
    /// the heap is far too small rather than that anything went wrong here.
    pub fn object_prototype(&mut self) -> Result<Value, RuntimeError> {
        let object = self
            .isolate
            .object_prototype()
            .ok_or(RuntimeError::OutOfMemory)?;
        Ok(Value::from_slot(object.slot(), self.isolate.cage()))
    }

    /// An object with no properties that inherits from `prototype`, where `null` means nothing.
    ///
    /// This is `Object.create` without the second argument, and it is the only way to make an object
    /// whose prototype is not `Object.prototype` until `new` arrives.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Type`] if `prototype` is neither an object nor `null`, in the words
    /// Node uses, because this is the one place that has to render the offending value and putting
    /// the message anywhere else would mean writing it twice. Returns
    /// [`RuntimeError::OutOfMemory`] if the heap has no room for the object or its root shape.
    pub fn new_object_with_prototype(&mut self, prototype: Value) -> Result<Value, RuntimeError> {
        let above = if prototype.is_null() {
            None
        } else if let Some(object) = self.as_object(prototype) {
            Some(object)
        } else {
            let what = self.display(prototype);
            return Err(RuntimeError::Type(format!(
                "Object prototype may only be an Object or null: {what}"
            )));
        };
        let shape = self
            .isolate
            .root_shape_for(above)
            .ok_or(RuntimeError::OutOfMemory)?;
        let object =
            ObjectRef::new(self.isolate.heap_mut(), shape, 0).ok_or(RuntimeError::OutOfMemory)?;
        Ok(Value::from_slot(object.slot(), self.isolate.cage()))
    }

    /// Whether calling this value would work.
    ///
    /// True for a function written in JavaScript and for one written in Rust. There is nothing else
    /// callable yet, and when there is, this is the one place that has to learn about it.
    #[must_use]
    pub fn is_callable(&self, value: Value) -> bool {
        self.as_closure(value).is_some() || self.as_native(value).is_some()
    }

    /// The own properties of an object, in the order they were added, or `None` if it is not one.
    ///
    /// Insertion order rather than any other order, because that is the order the language
    /// guarantees for string keys that are not array indices, and it is the order `JSON.stringify`
    /// and `Object.keys` both have to produce. It falls out of shapes for free: a shape is a
    /// transition chain and the chain is in the order the properties arrived.
    ///
    /// `None` and not an empty vector for a non object, because a caller usually has something
    /// different to do with a number than with an object that happens to have no properties.
    ///
    /// The flags come back with each property because the value cannot be understood without them.
    /// An accessor's slot holds a pair of functions rather than the property's value, and a caller
    /// that treats it as a value gets a heap pointer it will do something wrong with. Every caller
    /// today has to refuse when it meets one, and handing them the flags is what lets them notice.
    #[must_use]
    pub fn own_properties(&self, value: Value) -> Option<Vec<(String, Value, Attributes)>> {
        let object = self.as_object(value)?;
        let cage = self.isolate.cage();
        Some(
            object
                .enumerable(cage)
                .into_iter()
                .map(|(name, index, attributes)| {
                    let slot = object
                        .value_at(cage, index)
                        .expect("a name at this index means there is a value at it");
                    (
                        name.to_utf8_lossy(cage).into_owned(),
                        Value::from_bits(slot),
                        attributes,
                    )
                })
                .collect(),
        )
    }

    /// Whether this value is an ordinary object, meaning something a property can be defined on.
    ///
    /// False for a function, which in this build is a native or a closure and not an object, and
    /// false for every primitive. A builtin asks this when the specification says "if Type(O) is not
    /// Object, throw", because the message it throws names the builtin and so cannot come from here.
    /// Ordinary in the specification's sense, meaning an object with the standard behaviour, which
    /// is every object this build can make.
    #[must_use]
    pub fn is_ordinary_object(&self, value: Value) -> bool {
        self.as_object(value).is_some()
    }

    /// `ToBoolean`, for a builtin reading a flag out of an object a program wrote.
    #[must_use]
    pub fn is_truthy(&self, value: Value) -> bool {
        self.truthy(value)
    }

    /// `SameValue`, which is `===` with the two cases it gets wrong for this purpose put right.
    ///
    /// `NaN` is the same value as `NaN` and positive zero is not the same value as negative zero,
    /// which is the opposite of what `===` says about both. The specification uses this and not `===`
    /// when it asks whether redefining a non writable property is actually changing it, so a
    /// `defineProperty` that writes the same `NaN` back is allowed and one that turns a positive zero
    /// into a negative one is not.
    #[must_use]
    pub fn same_value(&self, left: Value, right: Value) -> bool {
        if let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) {
            if left.is_nan() && right.is_nan() {
                return true;
            }
            return left == right && left.is_sign_negative() == right.is_sign_negative();
        }
        self.strict_equal(left, right)
    }

    /// Read a property the way a program would, walking the prototype chain.
    ///
    /// `None` means the name is nowhere on the chain, which is a different answer from `undefined`
    /// being found there. A property descriptor needs that distinction and `.` throws it away:
    /// `{}` and `{value: undefined}` describe different properties, so a builtin reading a
    /// descriptor cannot use the operator a program would use.
    ///
    /// Takes `&mut self` for the reason [`Interpreter::global`] does, which is that a name given as
    /// text has to be interned before it can be compared against a name already on an object.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Unsupported`] when the name is found and it is an accessor. Reading
    /// one means calling the getter, and a builtin cannot call a JavaScript function: the loop is the
    /// only thing that can push a frame and a native has already left it. Refusing is the only
    /// correct answer available. Answering with the pair would hand a native a heap pointer it would
    /// treat as the property's value, and answering `undefined` would be a legal looking value that a
    /// program would go on to use.
    pub fn lookup(&mut self, object: Value, name: &str) -> Result<Option<Value>, RuntimeError> {
        let Some(interned) = self.isolate.intern(name) else {
            return Ok(None);
        };
        let interned = interned.as_string();
        let cage = self.isolate.cage();
        let mut holder = self.as_object(object);
        while let Some(current) = holder {
            if let Some((index, attributes)) = current.find(cage, interned) {
                if attributes.is_accessor() {
                    return Err(Self::native_met_an_accessor(name));
                }
                let bits = current
                    .value_at(cage, index)
                    .expect("a property the shape has is a property the object has room for");
                return Ok(Some(Value::from_bits(bits)));
            }
            holder = current.prototype(cage);
        }
        Ok(None)
    }

    /// What a builtin says when the property it needed to read turns out to be an accessor.
    ///
    /// One message in one place, because every builtin that reads a property hits the same wall for
    /// the same reason and they should all say so in the same words. It names the property, because
    /// the whole value of refusing by name is that the person reading it knows which one.
    #[cold]
    #[inline(never)]
    #[must_use]
    pub fn native_met_an_accessor(name: &str) -> RuntimeError {
        RuntimeError::Unsupported(format!(
            "reading '{name}' means calling a getter, and a builtin written in Rust cannot call a \
             JavaScript function yet"
        ))
    }

    /// The two halves of an accessor pair, for a builtin that has to report them rather than call
    /// them.
    ///
    /// `Object.getOwnPropertyDescriptor` needs this and it is the only thing that legitimately does.
    /// Reporting a getter is not calling it, so this is allowed where [`Interpreter::lookup`] is not.
    ///
    /// Returns `(None, None)` for anything that is not a pair, which a caller reaches only by asking
    /// about a property whose attributes did not say it was an accessor.
    #[must_use]
    pub fn accessor_halves(&self, pair: Value) -> (Option<Value>, Option<Value>) {
        let cage = self.isolate.cage();
        let Some(pair) = pair.to_slot(cage).and_then(AccessorPairRef::from_slot) else {
            return (None, None);
        };
        (
            pair.getter(cage).map(|slot| Value::from_slot(slot, cage)),
            pair.setter(cage).map(|slot| Value::from_slot(slot, cage)),
        )
    }

    /// Put a getter and a setter in the heap, for a builtin that is defining an accessor.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the pair.
    pub fn accessor_pair(
        &mut self,
        getter: Option<Value>,
        setter: Option<Value>,
    ) -> Result<Value, RuntimeError> {
        let cage = self.isolate.cage();
        let getter = getter.and_then(|value| value.to_slot(cage));
        let setter = setter.and_then(|value| value.to_slot(cage));
        let pair = AccessorPairRef::new(self.isolate.heap_mut(), getter, setter)
            .ok_or(RuntimeError::OutOfMemory)?;
        Ok(Value::from_slot(pair.slot(), self.isolate.cage()))
    }

    /// The own property `name`, with what it is allowed to do, or `None` if there is not one.
    ///
    /// Own and not inherited, which is the question `Object.getOwnPropertyDescriptor` asks and the
    /// one [`Interpreter::lookup`] does not.
    pub fn own_descriptor(&mut self, object: Value, name: &str) -> Option<(Value, Attributes)> {
        let name = self.isolate.intern(name)?.as_string();
        let object = self.as_object(object)?;
        let cage = self.isolate.cage();
        let (index, attributes) = object.find(cage, name)?;
        let bits = object.value_at(cage, index)?;
        Some((Value::from_bits(bits), attributes))
    }

    /// Define a property, which is not the same operation as assigning to one.
    ///
    /// Assignment asks the prototype chain for permission and this does not, assignment cannot change
    /// what a property is allowed to do and this can, and assignment on a read only property fails
    /// where this succeeds. Everything a program can reach this through goes past a set of rules
    /// first, in `Object.defineProperty`, and none of those rules are here: this puts the value and
    /// the three flags where it is told to.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Type`] if `object` is not an object. Every builtin that gets here has
    /// already checked that and thrown a message naming itself, so this is the answer for a caller
    /// that did not, and it is deliberately not one of the messages Node prints. Returns
    /// [`RuntimeError::OutOfMemory`] if the heap has no room for the name or the new shape.
    pub fn define_property(
        &mut self,
        object: Value,
        name: &str,
        value: Value,
        attributes: Attributes,
    ) -> Result<(), RuntimeError> {
        let Some(target) = self.as_object(object) else {
            return Err(RuntimeError::Type(
                "Cannot define a property on a value that is not an object".to_owned(),
            ));
        };
        let name = self
            .isolate
            .intern(name)
            .ok_or(RuntimeError::OutOfMemory)?
            .as_string();
        target
            .define(self.isolate.heap_mut(), name, value.to_bits(), attributes)
            .ok_or(RuntimeError::OutOfMemory)
    }

    /// Define one half of an accessor written in an object literal.
    ///
    /// The two halves of `{get x() {}, set x(v) {}}` are two entries in the source and one property
    /// in the result, so the second half to arrive has to find the first and join it rather than
    /// replace it. Anything else that is already sitting under that name is replaced outright,
    /// including a data property, because a literal defines its properties in source order and the
    /// last mention of a name is the one that survives.
    ///
    /// Both flags are true. That is the difference between the notation and `Object.defineProperty`,
    /// where every flag a descriptor does not mention defaults to false.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the pair or the new shape.
    fn define_half(
        &mut self,
        object: Value,
        name: StringRef,
        function: Value,
        half: AccessorHalf,
    ) -> Result<(), RuntimeError> {
        let Some(target) = self.as_object(object) else {
            return Ok(());
        };
        let cage = self.isolate.cage();
        let existing = target
            .find(cage, name)
            .filter(|(_, attributes)| attributes.is_accessor())
            .and_then(|(index, _)| target.value_at(cage, index))
            .map(Value::from_bits);
        let (getter, setter) = existing.map_or((None, None), |pair| self.accessor_halves(pair));
        let (getter, setter) = match half {
            AccessorHalf::Getter => (Some(function), setter),
            AccessorHalf::Setter => (getter, Some(function)),
        };
        let pair = self.accessor_pair(getter, setter)?;
        target
            .define(
                self.isolate.heap_mut(),
                name,
                pair.to_bits(),
                Attributes::accessor(true, true),
            )
            .ok_or(RuntimeError::OutOfMemory)
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
        let unit = self.load(blueprint)?;
        self.stack.push(blueprint.frame_size, &[], 0)?;
        let outcome = self.execute(&unit);
        // Unconditionally, because a `return` at the top level pops the last frame itself and an
        // error can stop anywhere with any number of frames still live. Either way the interpreter
        // is left with an empty stack and is usable again, rather than carrying dead frames the next
        // call would trip over.
        self.stack.unwind();
        match outcome {
            // The last point at which a thrown value and the heap it lives in are both in reach.
            // Past here the value is an address nobody can read, so it becomes text on the way out.
            Err(RuntimeError::Thrown(value)) => Err(RuntimeError::Uncaught(self.display(value))),
            other => other,
        }
    }

    /// Flatten a blueprint tree into a unit, resolving every function's constant pool on the way.
    ///
    /// The pool holds Rust strings because the pass that built it could not reach the atom table,
    /// which is the arrangement written down at the top of `katsu-ir`'s constant module. This is the
    /// other half of it: one walk that interns every string and boxes every number, so that a
    /// `load_const` is an index into a slice rather than a hash of a literal.
    ///
    /// String constants are interned rather than merely allocated, because a literal is exactly the
    /// kind of string a program mentions repeatedly and every duplicate saved is a duplicate the
    /// collector never has to walk.
    ///
    /// Every function is resolved, not only the top level, which is the part that changed when calls
    /// arrived. A string literal inside a function body used to be unreachable, because there was no
    /// way to get into a function body.
    fn load<'a>(&mut self, root: &'a FunctionBlueprint) -> Result<Unit<'a>, RuntimeError> {
        // The closure borrows the isolate rather than all of `self`, because `Unit::load` holds it
        // for the whole walk and the stack has to stay reachable.
        let isolate = &mut self.isolate;
        let mut intern = |text: &str| -> Result<Value, RuntimeError> {
            let atom = isolate.intern(text).ok_or(RuntimeError::OutOfMemory)?;
            Ok(Value::from_slot(atom.as_string().slot(), isolate.cage()))
        };
        Unit::load(root, |blueprint| {
            let constants = blueprint
                .constants
                .values()
                .iter()
                .map(|constant| match constant {
                    Constant::Number(number) => Ok(Value::from_f64(*number)),
                    Constant::String(text) => intern(text),
                })
                .collect::<Result<Vec<Value>, RuntimeError>>()?;
            // The empty value rather than an interned empty string, because "this function has no
            // name" and "this function is called the empty string" print differently and only the
            // first one is what an anonymous function means.
            let name = if blueprint.name.is_empty() {
                Value::EMPTY
            } else {
                intern(&blueprint.name)?
            };
            Ok(Resolved { constants, name })
        })
    }

    /// The loop.
    ///
    /// Long because the instruction set is long, and one arm per opcode with no shared helper is
    /// the point rather than an accident. `match_same_arms` is off for the same reason: `load_this`
    /// and `load_undefined` happen to produce the same value today and stop doing so the moment a
    /// function can be called with a receiver, so merging them would be merging two different
    /// questions that currently have the same answer.
    ///
    /// The three variables above the loop are the machine state a call and a return change: which
    /// function is running, its code and its constants. They are locals rather than reads through
    /// the frame because every instruction touches at least one of them, and a call is the only
    /// thing that moves them.
    ///
    /// `needless_continue` is off because the `continue` it points at is the one inside `raise!`,
    /// and that `continue` is what makes the macro diverge so that it can be written in expression
    /// position and in a `let ... else`. It is redundant only at the arms where the raise happens to
    /// be the last thing in the loop body, and removing it there would mean two spellings of the
    /// same macro chosen by where it is called from.
    #[allow(
        clippy::too_many_lines,
        clippy::match_same_arms,
        clippy::needless_continue
    )]
    fn execute(&mut self, unit: &Unit<'_>) -> Result<Value, RuntimeError> {
        let mut function = 0_u32;
        let mut blueprint = unit.function(function).blueprint;
        let mut constants = unit.function(function).constants.as_slice();
        let mut pc = 0_usize;

        // `?` is the wrong thing to do with an exception, because it leaves the function and every
        // handler in the program with it. These two put it back: `raise!` hands the error to the
        // unwinder and carries on running wherever that lands, and `guard!` is the same thing
        // spelled for a call that returns a `Result`. The `?` inside `raise!` is then exactly the
        // uncaught path, which is the one case where leaving is right.
        //
        // Macros rather than a method because the four variables above the loop are what an unwind
        // changes, and a method cannot assign to them. They are defined here rather than at the top
        // of the file so that they can name those variables, which is what makes every call site
        // one word instead of five lines, and the `continue` in `raise!` belongs to the loop it is
        // written inside rather than to anything in the macro.
        macro_rules! raise {
            ($error:expr) => {{
                let landing = self.handle(unit, $error, function, pc)?;
                function = landing.function;
                blueprint = unit.function(function).blueprint;
                constants = unit.function(function).constants.as_slice();
                pc = landing.pc;
                continue;
            }};
        }
        macro_rules! guard {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(error) => raise!(error),
                }
            };
        }

        loop {
            // A verified blueprint ends in a terminator and has no jump target past its end, so
            // running off the end is impossible and the index is a bug in lowering rather than in
            // the program. Panicking here would be the same claim with a worse message.
            let op = *blueprint
                .code
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
                        raise!(RuntimeError::Reference(format!(
                            "Cannot access '{name}' before initialization"
                        )));
                    }
                }
                Op::ThrowConstAssignment => {
                    raise!(RuntimeError::Type(
                        "Assignment to constant variable.".to_owned(),
                    ));
                }

                // A method call put the object here and this reads it back. Everything else is a
                // question the call site could not answer, so it is answered here.
                Op::LoadThis { dst } => {
                    let receiver = guard!(self.receiver(blueprint));
                    self.stack.set(dst, receiver);
                }

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
                        _ => guard!(self.add_slowly(left, right)),
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
                    let equal = guard!(self.loose_equal(self.stack.get(lhs), self.stack.get(rhs)));
                    self.stack.set(dst, Value::from_bool(equal));
                }
                Op::NotEqual { dst, lhs, rhs, .. } => {
                    let equal = guard!(self.loose_equal(self.stack.get(lhs), self.stack.get(rhs)));
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
                    let less = guard!(self.less_than(self.stack.get(lhs), self.stack.get(rhs)));
                    self.stack.set(dst, Value::from_bool(less.unwrap_or(false)));
                }
                Op::Greater { dst, lhs, rhs, .. } => {
                    // The operands go in the other way round, which is what makes `>` a `<` with its
                    // arguments swapped rather than a comparison of its own.
                    let less = guard!(self.less_than(self.stack.get(rhs), self.stack.get(lhs)));
                    self.stack.set(dst, Value::from_bool(less.unwrap_or(false)));
                }
                Op::LessEqual { dst, lhs, rhs, .. } => {
                    let less = guard!(self.less_than(self.stack.get(rhs), self.stack.get(lhs)));
                    self.stack.set(dst, Value::from_bool(!less.unwrap_or(true)));
                }
                Op::GreaterEqual { dst, lhs, rhs, .. } => {
                    let less = guard!(self.less_than(self.stack.get(lhs), self.stack.get(rhs)));
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
                    let value = guard!(self.type_of_value(self.stack.get(src)));
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

                // A closure is the function plus the environment that was live where it was
                // written, which is this frame's context. Capturing at the point the closure is made
                // rather than at the point it is called is the whole of what a closure is.
                Op::NewClosure {
                    dst,
                    blueprint: nested,
                } => {
                    let target = unit.child(function, nested.0);
                    let captured = self.frame_context();
                    let name = self.as_string(unit.function(target).name);
                    let closure = ClosureRef::new(self.isolate.heap_mut(), target, captured, name)
                        .ok_or(RuntimeError::OutOfMemory)?;
                    let value = Value::from_slot(closure.slot(), self.isolate.cage());
                    self.stack.set(dst, value);
                }

                // An empty object with room for the properties the stores after this one are about
                // to put in it. It starts at the root shape and walks down the transition tree one
                // store at a time, which is what makes a literal and an object grown a property at
                // a time reach the same shape.
                Op::NewObject { dst, slots } => {
                    let object = self.new_object(usize::from(slots))?;
                    let value = Value::from_slot(object.slot(), self.isolate.cage());
                    self.stack.set(dst, value);
                }

                // A new level of environment, nested inside whatever this frame already had. The
                // cells start as the hole and not as `undefined`, because a `let` that a nested
                // function reads is in its dead zone until its declaration runs, and the hole is how
                // the dead zone is spelled everywhere else in this loop.
                Op::NewContext { size } => {
                    let parent = self.frame_context();
                    let context = ContextRef::new(
                        self.isolate.heap_mut(),
                        parent,
                        u32::from(size),
                        Value::EMPTY.to_bits(),
                    )
                    .ok_or(RuntimeError::OutOfMemory)?;
                    self.set_frame_context(Some(context));
                }

                Op::LoadUpvalue { dst, hops, slot } => {
                    let value = guard!(self.upvalue(hops, slot));
                    self.stack.set(dst, value);
                }
                Op::StoreUpvalue { hops, slot, src } => {
                    let value = self.stack.get(src);
                    let context = guard!(self.context_at(hops));
                    if !context.set_cell(self.isolate.heap_mut(), u32::from(slot), value.to_bits())
                    {
                        raise!(Self::broken_environment());
                    }
                }

                // A name nothing lexical claimed. The constant is already interned, because the
                // whole pool was interned when the unit was loaded, so the lookup is a hash of four
                // bytes rather than of the text.
                Op::LoadGlobal { dst, name, .. } => {
                    let key = self.name_at(constants, name);
                    let Some(value) = self.isolate.globals().get(key) else {
                        raise!(RuntimeError::Reference(format!(
                            "{} is not defined",
                            Self::constant_name(blueprint, name)
                        )));
                    };
                    self.stack.set(dst, value);
                }

                // The one read in the language that is allowed to miss. `typeof nothing` is
                // `"undefined"` and not a `ReferenceError`, which is why this cannot be the load
                // above with a `TypeOf` after it.
                Op::LoadGlobalForTypeof { dst, name, .. } => {
                    let key = self.name_at(constants, name);
                    let value = self.isolate.globals().get(key).unwrap_or(Value::UNDEFINED);
                    self.stack.set(dst, value);
                }

                // This creates the binding if there was not one, which is what an assignment to an
                // undeclared name does in sloppy mode. Strict mode throws instead, and that is a
                // check for the day lowering tells the interpreter which mode it is in.
                Op::StoreGlobal { name, src, .. } => {
                    let key = self.name_at(constants, name);
                    let value = self.stack.get(src);
                    self.isolate.globals_mut().set(key, value);
                }

                Op::Call {
                    dst,
                    callee,
                    args,
                    argc,
                    ..
                } => {
                    let target = self.stack.get(callee);
                    let Some(closure) = self.as_closure(target) else {
                        // A call that leaves the loop entirely, which is how anything the runtime
                        // implements in Rust gets reached. It is here rather than in its own opcode
                        // because a program cannot tell the two apart: `print` is a value that was
                        // called, and which language its body is written in is not something the
                        // call site knows.
                        if let Some(native) = self.as_native(target) {
                            let value = guard!(self.call_native(native, None, args, argc));
                            self.stack.set(dst, value);
                            continue;
                        }
                        // Node names the expression that was called and this names the value it
                        // produced, so `x()` on a five reports `5 is not a function` where Node
                        // reports `x is not a function`. Naming the expression means keeping the
                        // source span of every call site, which is the same work that makes a
                        // function stringify as its source, and both land together.
                        let text = self.display(target);
                        raise!(RuntimeError::Type(format!("{text} is not a function")));
                    };
                    let index = closure.function(self.isolate.cage());
                    let captured = closure.captured(self.isolate.cage());
                    let callee = unit.function(index).blueprint;
                    // The resume point is the instruction after the call, and `pc` is already
                    // there because it was advanced before the match. It is recorded on the frame
                    // being pushed rather than the one being left, so a return is one pop and a
                    // read rather than a search back through the stack.
                    //
                    // A push that fails is a stack overflow, and that is an exception like any
                    // other: node catches `RangeError: Maximum call stack size exceeded` in a
                    // `try` and so does this. The frame was not pushed, so the search starts in
                    // the frame that made the call, which is where the error happened.
                    let return_pc = u32::try_from(pc).map_err(|_| Self::code_too_long())?;
                    guard!(
                        self.stack
                            .push_call(
                                callee.frame_size,
                                Invocation {
                                    arity: callee.arity,
                                    first: args,
                                    passed: argc,
                                    function: index,
                                    return_pc,
                                    return_to: dst,
                                    // A plain call supplies no receiver. What `this` means inside
                                    // one is a question about where the code is rather than about
                                    // the call, and `Op::LoadThis` is where that is answered.
                                    receiver: Value::EMPTY,
                                },
                            )
                            .map_err(RuntimeError::from)
                    );
                    self.set_frame_context(captured);
                    function = index;
                    blueprint = callee;
                    constants = unit.function(index).constants.as_slice();
                    pc = 0;
                }

                Op::Return { src } => {
                    let value = self.stack.get(src);
                    let frame = self
                        .stack
                        .pop()
                        .expect("a return happens inside the frame it returns from");
                    // Nothing underneath means the outermost function returned, which is the value
                    // the embedder asked for.
                    let Some(caller) = self.stack.current().copied() else {
                        return Ok(value);
                    };
                    // A setter is called for what it does and its return value is specified to be
                    // discarded, so the frame it returns into has nowhere to put one. The test is
                    // here rather than in a second opcode because it is false for every call a
                    // program writes and a branch that is never taken costs nothing.
                    if frame.return_to != Register::NOWHERE {
                        self.stack.set(frame.return_to, value);
                    }
                    function = caller.function;
                    blueprint = unit.function(function).blueprint;
                    constants = unit.function(function).constants.as_slice();
                    pc = frame.return_pc as usize;
                }

                // The operation the whole architecture is judged on, in the form it has before there
                // is a cache to fill. The cache operand is carried and ignored: there is a shape to
                // compare against now, which is the thing that was missing, and filling it is the
                // next piece of work rather than this one. What it costs until then is that every
                // read walks the shape chain, which is the cost the module documentation in
                // `shape.rs` is explicit about.
                Op::GetProp { dst, obj, key, .. } => {
                    let object = self.stack.get(obj);
                    let name = self.name_at(constants, key);
                    match guard!(self.property(object, name, blueprint, key)) {
                        Found::Value(value) => self.stack.set(dst, value),
                        // A read that is a call, kept to two instructions here and done in a cold
                        // function. Everything this needs to say lives in registers the loop holds
                        // across every other opcode, so writing it out inline puts more pressure on
                        // them than the arm is worth: it runs once per accessor and never on the
                        // path the architecture is judged on.
                        Found::Getter(getter) => {
                            if let Some((index, callee)) =
                                guard!(self.read_through(getter, object, dst, pc, unit))
                            {
                                function = index;
                                blueprint = callee;
                                constants = unit.function(index).constants.as_slice();
                                pc = 0;
                            }
                        }
                    }
                }

                // A store either writes a property the object has or adds one it does not, and until
                // there were shapes the second of those was a refusal. Adding one takes a shape
                // transition, so the store that grows an object is the store that allocates.
                //
                // A store to something that is not an object is where the two modes of the language
                // genuinely differ. Outside strict mode it is a no operation, because the wrapper
                // object the write would land on is thrown away on the next line, and inside it that
                // silence was judged to be a bug worth reporting. `undefined` and `null` throw
                // either way, because there is nothing to wrap in the first place.
                Op::SetProp {
                    obj, key, value, ..
                } => {
                    let object = self.stack.get(obj);
                    let name = self.name_at(constants, key);
                    let new = self.stack.get(value);
                    if object.is_nullish() {
                        raise!(Self::nothing_to_write(object, blueprint, key));
                    }
                    match self.as_object(object) {
                        Some(target) => match guard!(self.assign(target, name, new)) {
                            Wrote::Yes => {}
                            Wrote::Refused if blueprint.strict => {
                                raise!(self.read_only(target, blueprint, key));
                            }
                            Wrote::NoSetter if blueprint.strict => {
                                raise!(self.only_a_getter(target, blueprint, key));
                            }
                            Wrote::Refused | Wrote::NoSetter => {}
                            // A write that is a call. The one argument is the value being assigned,
                            // which is already in a register, so it is passed where it sits. The
                            // answer goes nowhere: a setter's return value is specified to be
                            // discarded, and the value of the assignment expression is the value
                            // that went in rather than anything the setter said about it.
                            Wrote::Setter(setter) => {
                                let return_pc =
                                    u32::try_from(pc).map_err(|_| Self::code_too_long())?;
                                let call = Call {
                                    target: setter,
                                    receiver: object,
                                    first: value,
                                    passed: 1,
                                    return_to: Register::NOWHERE,
                                    return_pc,
                                };
                                match guard!(self.enter(call, unit)) {
                                    Entered::Frame { index, callee } => {
                                        function = index;
                                        blueprint = callee;
                                        constants = unit.function(index).constants.as_slice();
                                        pc = 0;
                                    }
                                    Entered::Answered(_) => {}
                                }
                            }
                        },
                        None if blueprint.strict => {
                            raise!(self.nowhere_to_write(object, blueprint, key));
                        }
                        None => {}
                    }
                }

                // A property of an object literal. The object is the one being built a few
                // instructions ago, so it is always an object and the nullish checks that guard a
                // store have nothing to do here.
                Op::DefineValue {
                    obj, key, value, ..
                } => {
                    let object = self.stack.get(obj);
                    let name = self.name_at(constants, key);
                    let new = self.stack.get(value);
                    if let Some(target) = self.as_object(object) {
                        guard!(
                            target
                                .define(
                                    self.isolate.heap_mut(),
                                    name,
                                    new.to_bits(),
                                    Attributes::DEFAULT
                                )
                                .ok_or(RuntimeError::OutOfMemory)
                        );
                    }
                }

                // Half an accessor in an object literal, with the same reasoning as above about the
                // object being known good.
                Op::DefineAccessor {
                    obj,
                    key,
                    value,
                    half,
                } => {
                    let object = self.stack.get(obj);
                    let name = self.name_at(constants, key);
                    let function = self.stack.get(value);
                    guard!(self.define_half(object, name, function, half));
                }

                // A property read and a call, kept in one opcode so that the receiver is not lost
                // between them. The object the property was read from goes on the frame the call
                // pushes, or straight into the native, and either way `this` inside the callee is
                // the object rather than nothing. That is the whole reason this is one opcode and
                // not a read followed by a call.
                Op::CallMethod {
                    dst,
                    obj,
                    key,
                    args,
                    argc,
                    ..
                } => {
                    let object = self.stack.get(obj);
                    let name = self.name_at(constants, key);
                    let target = match guard!(self.property(object, name, blueprint, key)) {
                        Found::Value(value) => value,
                        // `o.x()` where `x` is a getter is two calls, and the second one cannot be
                        // set up until the first has returned. The loop has no way to hold a call
                        // that is waiting on another call, because the only thing it resumes into
                        // is the instruction after the one it left, so this refuses by name rather
                        // than calling the getter and then losing its answer.
                        Found::Getter(_) => {
                            let key = Self::constant_name(blueprint, key);
                            raise!(RuntimeError::Unsupported(format!(
                                "calling '{key}', which is a getter, needs the result of the \
                                 getter to become the callee and the interpreter has nowhere to \
                                 keep a call that is waiting on another call"
                            )));
                        }
                    };
                    let Some(closure) = self.as_closure(target) else {
                        if let Some(native) = self.as_native(target) {
                            let value = guard!(self.call_native(native, Some(object), args, argc));
                            self.stack.set(dst, value);
                            continue;
                        }
                        // Node names the whole expression here, as `console.nope is not a function`,
                        // and naming the object half of it needs the same source spans that the plain
                        // call arm is waiting on. The property is named because that half is a
                        // constant this opcode is already holding.
                        let key = Self::constant_name(blueprint, key);
                        raise!(RuntimeError::Type(format!("{key} is not a function")));
                    };
                    let index = closure.function(self.isolate.cage());
                    let captured = closure.captured(self.isolate.cage());
                    let callee = unit.function(index).blueprint;
                    let return_pc = u32::try_from(pc).map_err(|_| Self::code_too_long())?;
                    guard!(
                        self.stack
                            .push_call(
                                callee.frame_size,
                                Invocation {
                                    arity: callee.arity,
                                    first: args,
                                    passed: argc,
                                    function: index,
                                    return_pc,
                                    return_to: dst,
                                    // The object the method was read from, which is the whole
                                    // point of this opcode existing separately from `Op::Call`.
                                    receiver: object,
                                },
                            )
                            .map_err(RuntimeError::from)
                    );
                    self.set_frame_context(captured);
                    function = index;
                    blueprint = callee;
                    constants = unit.function(index).constants.as_slice();
                    pc = 0;
                }

                // The instruction that hands a value to the unwinder. Where it goes is not written
                // here and could not be: entering the `try` emitted nothing at all, so the handler
                // table is the only thing that knows, and it is read now or never.
                Op::Throw { src } => {
                    let value = self.stack.get(src);
                    raise!(RuntimeError::Thrown(value));
                }

                // Everything that still needs an object that can grow, which is object literals,
                // indexed access and `delete`. Named rather than silently wrong.
                _ => return Err(RuntimeError::NotImplemented(op)),
            }
        }
    }

    /// Find the handler for an error and put the machine where that handler starts.
    ///
    /// The search is the price of the design. Entering a `try` costs nothing, because there is
    /// nothing to enter: the ranges were written into the function when it was lowered, and this
    /// walks them when something actually throws. `spec/04-frontend.md` took that trade the way the
    /// JVM took it, on the grounds that a `try` inside a hot loop is common and a throw inside one
    /// is not.
    ///
    /// It searches across frames and not only inside one, which is the whole reason a `throw` deep
    /// in a call is any use. Every frame with no handler for the instruction it stopped at is
    /// popped, and the frame underneath resumes at the call that got it into this, which is exactly
    /// the instruction that needs looking up next.
    ///
    /// # Errors
    ///
    /// Returns the error unchanged if nothing catches it, and also if nothing could have: out of
    /// memory, an interrupt and an unimplemented opcode are things that happened to the program
    /// rather than things the program did, and none of them is catchable in Node either.
    fn handle(
        &mut self,
        unit: &Unit<'_>,
        error: RuntimeError,
        function: u32,
        pc: usize,
    ) -> Result<Landing, RuntimeError> {
        if !Self::catchable(&error) {
            return Err(error);
        }
        let mut function = function;
        let mut pc = pc;
        loop {
            // `pc` was moved past the instruction before the match ran, and a frame unwound into
            // resumes after the call it made, so in both cases the instruction that threw is the
            // one before `pc` and `pc` is at least one.
            let at = katsu_ir::CodeOffset(
                u32::try_from(pc - 1).expect("a pc came from a code offset, which is a u32"),
            );
            if let Some(handler) = unit.function(function).blueprint.handler_for(at).copied() {
                // The value is built here and nowhere earlier, so a `TypeError` that nobody catches
                // never pays for the object it would have been.
                let value = self.exception(error)?;
                self.stack.set(handler.register, value);
                return Ok(Landing {
                    function,
                    pc: handler.target.0 as usize,
                });
            }
            let frame = self
                .stack
                .pop()
                .expect("an exception is always thrown inside a frame");
            let Some(caller) = self.stack.current().copied() else {
                return Err(error);
            };
            function = caller.function;
            pc = frame.return_pc as usize;
        }
    }

    /// Whether a `catch` is allowed to see this error at all.
    ///
    /// The four that are not are not JavaScript exceptions. A program cannot catch running out of
    /// memory under Node, it cannot catch being killed by a worker terminating, and an opcode or a
    /// builtin this build has not written yet is a gap in katsu rather than an event in the program,
    /// so letting a `catch` swallow one would turn a missing feature into a wrong answer.
    ///
    /// The unfinished builtin belongs in this list for a sharper reason than the others. A program
    /// that wraps `JSON.parse` in a `try` is doing something completely reasonable, because parsing
    /// really can fail on bad input, and if our refusal were catchable that program would take our
    /// gap for malformed input and carry on down its error path with a wrong answer and no
    /// indication anything was missing.
    const fn catchable(error: &RuntimeError) -> bool {
        !matches!(
            error,
            RuntimeError::NotImplemented(_)
                | RuntimeError::Unsupported(_)
                | RuntimeError::OutOfMemory
                | RuntimeError::Interrupted
        )
    }

    /// The value a `catch` binds, which is where an engine error stops being a message.
    ///
    /// A thrown value is already a value and passes straight through. The three the engine raises
    /// are a name and a message until this point, and become an object with those two as own
    /// properties, which is what `e.name` and `e.message` read.
    ///
    /// It is not an `Error` yet, in the sense that there is no `Error` to be an instance of:
    /// `e instanceof Error` needs an `Error` constructor with a prototype on it, which needs `new`,
    /// and `e.stack` needs the source spans that stack traces need, and both are ahead on M1. So `console.log(e)` prints the object rather than
    /// `TypeError: ...`, which is a difference from Node that is visible and deliberate rather than
    /// quietly wrong.
    fn exception(&mut self, error: RuntimeError) -> Result<Value, RuntimeError> {
        let (name, message) = match error {
            RuntimeError::Thrown(value) => return Ok(value),
            RuntimeError::Reference(message) => ("ReferenceError", message),
            RuntimeError::Type(message) => ("TypeError", message),
            RuntimeError::Range(message) => ("RangeError", message),
            // Unreachable, because `handle` returns the uncatchable ones before it starts looking
            // for a handler. Handing it back rather than panicking keeps that a fact about one
            // function instead of a claim two functions have to agree on.
            other => return Err(other),
        };
        // The name is interned and the message is not, on purpose. There are three names and a
        // program that throws will mention them again, while a message is built by `format!` and
        // is usually seen once, so hashing it would be work spent on a string with no second use.
        let name = self.intern(name)?;
        let message = self
            .isolate
            .allocate_string(&message)
            .ok_or(RuntimeError::OutOfMemory)?;
        let message = self.string_value(message);
        self.host_object(&[("name", name), ("message", message)])
    }

    /// The environment the running frame reads captured variables through.
    fn frame_context(&self) -> Option<ContextRef> {
        let bits = self
            .stack
            .current()
            .expect("something is running whenever an instruction is executing")
            .context;
        ContextRef::from_slot(katsu_gc::Slot::from_bits(bits))
    }

    /// Replace the running frame's environment, which a call and `new_context` both do.
    fn set_frame_context(&mut self, context: Option<ContextRef>) {
        self.stack
            .current_mut()
            .expect("something is running whenever an instruction is executing")
            .context = context.map_or(0, |context| context.slot().to_bits());
    }

    /// Walk `hops` levels out from the running frame's environment.
    ///
    /// `hops` counts levels of context and not levels of source nesting, because scope analysis only
    /// asks for a context when a nested function actually reads an outer variable. A function three
    /// blocks deep whose parents captured nothing is one hop from the outermost context, and that is
    /// what makes the walk short in real code rather than proportional to how deeply somebody
    /// indented.
    fn context_at(&self, hops: u16) -> Result<ContextRef, RuntimeError> {
        let mut context = self.frame_context().ok_or_else(Self::broken_environment)?;
        for _ in 0..hops {
            context = context
                .parent(self.isolate.cage())
                .ok_or_else(Self::broken_environment)?;
        }
        Ok(context)
    }

    /// Read one captured variable.
    fn upvalue(&self, hops: u16, slot: u16) -> Result<Value, RuntimeError> {
        let context = self.context_at(hops)?;
        let bits = context
            .cell(self.isolate.cage(), u32::from(slot))
            .ok_or_else(Self::broken_environment)?;
        Ok(Value::from_bits(bits))
    }

    /// The error for an environment that does not have the shape the bytecode expects.
    ///
    /// Not reachable from any JavaScript program, because scope analysis computed both the hop count
    /// and the slot number and lowering emitted them together. It is an error rather than a panic
    /// because the alternative to reporting a compiler bug is taking down the process of whoever
    /// happened to run into it.
    fn broken_environment() -> RuntimeError {
        RuntimeError::Reference(
            "an environment did not have the shape the bytecode expected, which is a bug in katsu \
             rather than in this program"
                .to_owned(),
        )
    }

    /// The error for a native object pointing at a table entry that is not there.
    ///
    /// Not reachable either. The table only grows and the only way to get one of these objects is to
    /// add an entry first, so this means the value came from another isolate's heap, which the type
    /// system already works to prevent. It is an error for the same reason as
    /// [`Interpreter::broken_environment`]: reporting a runtime bug is better than aborting on it.
    fn broken_native() -> RuntimeError {
        RuntimeError::Type(
            "a native function pointed at nothing, which is a bug in katsu rather than in this \
             program"
                .to_owned(),
        )
    }

    /// The error for a function whose code is longer than a return address can name.
    ///
    /// Four billion instructions in one function. Lowering would run out of memory long before a
    /// program got here, and it is checked anyway because the alternative to checking is a truncated
    /// return address that resumes somewhere plausible and wrong.
    fn code_too_long() -> RuntimeError {
        RuntimeError::Range(
            "a function has more instructions than katsu can return into".to_owned(),
        )
    }

    /// The one place a value becomes a closure.
    ///
    /// The companion to [`Interpreter::as_string`] and the reason that function stopped being a
    /// range check. There are three kinds of object in the cage now, so a pointer is no longer
    /// self describing and both of these read the kind word before they believe anything.
    fn as_closure(&self, value: Value) -> Option<ClosureRef> {
        let slot = value.to_slot(self.isolate.cage())?;
        match HeapKind::of(self.isolate.cage(), slot)? {
            HeapKind::Closure => ClosureRef::from_slot(slot),
            HeapKind::String
            | HeapKind::Context
            | HeapKind::Native
            | HeapKind::Object
            | HeapKind::Shape
            | HeapKind::Properties
            | HeapKind::AccessorPair => None,
        }
    }

    /// The one place a value becomes a function written in Rust.
    ///
    /// Only ever asked after [`Interpreter::as_closure`] has already said no, because a call to a
    /// function written in JavaScript is the common case by a wide margin and it should not pay for
    /// a second kind check to find that out.
    fn as_native(&self, value: Value) -> Option<NativeRef> {
        let slot = value.to_slot(self.isolate.cage())?;
        match HeapKind::of(self.isolate.cage(), slot)? {
            HeapKind::Native => NativeRef::from_slot(slot),
            HeapKind::String
            | HeapKind::Closure
            | HeapKind::Context
            | HeapKind::Object
            | HeapKind::Shape
            | HeapKind::Properties
            | HeapKind::AccessorPair => None,
        }
    }

    /// The one place a value becomes an object with properties on it.
    ///
    /// The check is that the first word holds a shape, which is one tag test on a word the property
    /// read is about to load anyway. Strings and functions have properties in the language and do
    /// not have them here, because those properties live on `String.prototype` and
    /// `Function.prototype`, and a primitive reaching its prototype means the wrapper conversion
    /// that is still ahead. Prototype chains themselves work: it is the two prototypes that do not
    /// exist, not the mechanism.
    fn as_object(&self, value: Value) -> Option<ObjectRef> {
        let slot = value.to_slot(self.isolate.cage())?;
        match HeapKind::of(self.isolate.cage(), slot)? {
            HeapKind::Object => ObjectRef::from_slot(slot),
            HeapKind::String
            | HeapKind::Closure
            | HeapKind::Context
            | HeapKind::Native
            | HeapKind::Shape
            | HeapKind::Properties
            | HeapKind::AccessorPair => None,
        }
    }

    /// Leave the dispatch loop and run some Rust.
    ///
    /// The arguments are copied out of the caller's registers before the call, because the native
    /// takes the whole interpreter and cannot be holding a slice of the stack while it does. Eight
    /// fit without allocating, which is more than any builtin takes, and the copy is what makes the
    /// difference between a native that can allocate and one that cannot.
    ///
    /// No frame is pushed. A native does not use registers and cannot yet call back into
    /// JavaScript, so a frame would be a push and a pop that nothing reads. What it costs is that a
    /// native is invisible to `depth` and will be invisible in a stack trace, and the note in
    /// `native.rs` says so.
    fn call_native(
        &mut self,
        native: NativeRef,
        receiver: Option<Value>,
        first: Register,
        count: u16,
    ) -> Result<Value, RuntimeError> {
        let ordinal = native.ordinal(self.isolate.cage());
        let Some(call) = self.isolate.natives().get(ordinal) else {
            return Err(Self::broken_native());
        };
        let arguments: SmallVec<[Value; 8]> = SmallVec::from_slice(self.stack.range(first, count));
        call(self, receiver, &arguments)
    }

    /// Make a call the program did not write, which today means calling an accessor.
    ///
    /// The two halves are the two halves of `Op::Call`, moved out of the loop because there are now
    /// three places that need them and duplicating a frame push three times is how the receiver ends
    /// up being wrong in one of them. `Op::Call` and `Op::CallMethod` are deliberately left alone:
    /// they are the hot opcodes, and they carry the arity and the frame size straight from the
    /// operand into the push without going through a struct first.
    ///
    /// A getter that is not a function throws in the same words a call to a non function throws in.
    /// That happens when a program hands `Object.defineProperty` a `get` that is not callable, which
    /// the definition itself is specified to reject, so it is a message that should be unreachable
    /// rather than one nobody wrote.
    ///
    /// # Errors
    ///
    /// A stack overflow from the push, whatever a native throws, or a `TypeError` for a target that
    /// cannot be called.
    fn enter<'a>(&mut self, call: Call, unit: &'a Unit) -> Result<Entered<'a>, RuntimeError> {
        let Some(closure) = self.as_closure(call.target) else {
            if let Some(native) = self.as_native(call.target) {
                let value =
                    self.call_native(native, Some(call.receiver), call.first, call.passed)?;
                return Ok(Entered::Answered(value));
            }
            let text = self.display(call.target);
            return Err(RuntimeError::Type(format!("{text} is not a function")));
        };
        let index = closure.function(self.isolate.cage());
        let captured = closure.captured(self.isolate.cage());
        let callee = unit.function(index).blueprint;
        self.stack.push_call(
            callee.frame_size,
            Invocation {
                arity: callee.arity,
                first: call.first,
                passed: call.passed,
                function: index,
                return_pc: call.return_pc,
                return_to: call.return_to,
                receiver: call.receiver,
            },
        )?;
        self.set_frame_context(captured);
        Ok(Entered::Frame { index, callee })
    }

    /// Turn a read that found a getter into the call it is.
    ///
    /// Out of line and cold because `Op::GetProp` is the opcode the whole architecture is judged on
    /// and this arm runs once per accessor and never on a plain read. Measured, it is worth nothing
    /// on its own: the loop is fast either way and the cost of accessors is elsewhere, in the extra
    /// word the lookup now reads. It stays out of line because a cold arm belongs out of line, not
    /// because a benchmark moved.
    ///
    /// The getter takes no arguments, so `first` points at a register nothing is copied from, and
    /// its answer goes where the read's answer was going to go. Nothing else about it is special:
    /// the frame is an ordinary frame and it comes back through the ordinary return.
    ///
    /// A getter written in Rust has already run by the time this returns, so its answer is stored
    /// here and the loop is told to carry on where it was. Answering `None` is that, and answering
    /// `Some` is a frame the loop has to switch into.
    ///
    /// # Errors
    ///
    /// A program long enough that its own program counter does not fit in the return address, a
    /// stack overflow from the push, or whatever a getter written in Rust throws.
    #[cold]
    #[inline(never)]
    fn read_through<'a>(
        &mut self,
        getter: Value,
        receiver: Value,
        dst: Register,
        pc: usize,
        unit: &'a Unit,
    ) -> Result<Option<(u32, &'a FunctionBlueprint)>, RuntimeError> {
        let return_pc = u32::try_from(pc).map_err(|_| Self::code_too_long())?;
        let call = Call {
            target: getter,
            receiver,
            first: dst,
            passed: 0,
            return_to: dst,
            return_pc,
        };
        match self.enter(call, unit)? {
            Entered::Frame { index, callee } => Ok(Some((index, callee))),
            Entered::Answered(value) => {
                self.stack.set(dst, value);
                Ok(None)
            }
        }
    }

    /// The interned name a name operand holds.
    ///
    /// Both halves of this are guaranteed by something that already ran. The index is inside the
    /// pool because the verifier checked every constant index, and the constant is an interned
    /// string because lowering only puts a name in a name operand and `load` interned every string
    /// in the pool. Neither is a thing a program can cause, so both are `expect` rather than an
    /// error a caller would have no way to act on.
    fn name_at(&self, constants: &[Value], index: katsu_ir::ConstIndex) -> StringRef {
        let value = *constants
            .get(index.0 as usize)
            .expect("verify checked every constant index against the pool");
        self.as_string(value)
            .expect("a name operand holds a string, and load interned every string in the pool")
    }

    /// The text `console.log` prints for a function, which is what Node prints for one.
    ///
    /// Node shows the name because that is almost always the only thing about a function that
    /// identifies it at a glance, and `[Function (anonymous)]` when there is no name rather than an
    /// empty pair of brackets that reads like a mistake.
    ///
    /// The name comes off the closure and not out of the unit, which is why it is stored on the
    /// closure at all. An embedder holding a value has no unit to look anything up in, and a
    /// function that printed as `[Function (anonymous)]` outside the run that made it would be
    /// reporting the API's ignorance as a fact about the program.
    fn function_text(&self, closure: ClosureRef) -> String {
        match closure.name(self.isolate.cage()) {
            Some(name) => format!(
                "[Function: {}]",
                name.to_utf8_lossy(self.isolate.cage()).into_owned()
            ),
            None => "[Function (anonymous)]".to_owned(),
        }
    }

    /// The text `console.log` prints for a function written in Rust.
    ///
    /// The same shape Node prints, because in Node these are the same thing: `console.log` is
    /// written in C++ over there and prints as `[Function: log]` all the same. A native whose
    /// ordinal has no entry prints as anonymous rather than panicking, because a broken print is a
    /// bad way to find out about a broken table.
    fn native_text(&self, native: NativeRef) -> String {
        match self
            .isolate
            .natives()
            .name(native.ordinal(self.isolate.cage()))
        {
            Some(name) => format!("[Function: {name}]"),
            None => "[Function (anonymous)]".to_owned(),
        }
    }

    /// Read one property, own or inherited.
    ///
    /// Three answers and not two. The name is found somewhere on the chain, or it is nowhere on the
    /// chain and the answer is `undefined` rather than an error, which is the rule the whole
    /// language is built on. Anything that is not an object is also `undefined`, because a number's
    /// properties come off `Number.prototype` and the wrapper prototypes are still ahead.
    /// `undefined` and `null` are the exception, and they throw, because those two have no prototype
    /// to reach for in any milestone and Node's message for it is the single most read error message
    /// in JavaScript.
    ///
    /// The walk terminates without a visited set, because a chain can only be built downwards: an
    /// object names its prototype when it is created and there is no `Object.setPrototypeOf` and no
    /// `__proto__` to point an existing one back up. The day either of those lands is the day this
    /// needs the cycle check that the specification's `SetPrototypeOf` performs, and the check
    /// belongs there rather than here, because paying for it on every read to catch something a
    /// write could have refused is the wrong end.
    ///
    /// The name is the interned one and the index is only for the message, which is why the message
    /// is built in a function of its own that a successful read never calls. Reading the constant
    /// back out of the pool allocates a `String`, and doing that on the way through every property
    /// read that worked cost more than the lookup itself did.
    ///
    /// The first step is written out rather than being the first turn of the loop, so that finding
    /// the name on the object itself is the same straight line of code it was before there was a
    /// chain to walk. Being able to look above an object is not free: `property/prop_load` is around
    /// a tenth slower than it was, measured by alternating two binaries so that a busy machine
    /// charged both of them equally. That machine could not pin the number tighter than that, and it
    /// could not tell this shape apart from the shorter one that puts the first step inside the
    /// loop, so the reason to prefer this one is that the common case reads as the common case
    /// rather than a benchmark that says so. The thing that takes the tenth back is an inline cache,
    /// which is the next item on M1, and which is the entire reason the prototype lives in the shape.
    ///
    /// Accessors added a second tenth to the same benchmark, and where it went was measured rather
    /// than guessed. `property/prop_load` is about a fifth slower than it was without them, over
    /// three separate runs of twenty four alternating pairs each, against a control in the same pairs
    /// that stayed at 1.02. Swapping this one line back to the lookup that does not read the flags
    /// put it at 1.00 and made the runtime wrong, which is what says the whole cost is here and none
    /// of it is in the opcode or in the shape of what gets returned. Both of those were tried and
    /// neither moved the number.
    ///
    /// It is the honest price of the question. A slot can hold a value or a pair of functions and
    /// nothing about the slot says which, so something has to be read that says, and the flags are
    /// the only thing that can. Packing them into the count word so the search loop reads one word
    /// instead of two was tried too: it moved a little of the cost from the read to the write and
    /// left the total where it was, so it was not kept.
    ///
    /// The same inline cache takes back this tenth and the earlier one together, and it takes this
    /// one back completely rather than partly. A cache that has matched a shape has established in
    /// that comparison that the slot is a plain value, because the flags are part of what the shape
    /// says, so the fast path does not read them and does not test them.
    #[inline]
    fn property(
        &self,
        object: Value,
        name: StringRef,
        blueprint: &FunctionBlueprint,
        key: katsu_ir::ConstIndex,
    ) -> Result<Found, RuntimeError> {
        if object.is_undefined() || object.is_null() {
            return Err(Self::nothing_to_read(object, blueprint, key));
        }
        let Some(record) = self.as_object(object) else {
            return Ok(Found::Value(Value::UNDEFINED));
        };
        let cage = self.isolate.cage();
        if let Some((index, attributes)) = record.find(cage, name) {
            let bits = record
                .value_at(cage, index)
                .expect("a property the shape has is a property the object has room for");
            return Ok(self.found(bits, attributes));
        }
        Ok(self.inherited(record, name))
    }

    /// What a lookup found, once the flags have been consulted.
    ///
    /// The flags are consulted rather than the bits, because an accessor pair is a heap pointer and
    /// there is nothing in a heap pointer that says which kind of thing it points at without a load.
    /// This is the payoff for putting the bit in the shape: an inline cache that has matched a shape
    /// has established the answer in the comparison it already made, so the fast path this is
    /// standing in for will not run the test at all.
    #[inline]
    fn found(&self, bits: u64, attributes: Attributes) -> Found {
        if !attributes.is_accessor() {
            return Found::Value(Value::from_bits(bits));
        }
        self.getter_of(Value::from_bits(bits))
    }

    /// The getter half of a pair, or `undefined` when the property has no getter.
    ///
    /// A property with only a setter reads as `undefined` and does not throw, which is the language
    /// saying that a write only property is still readable and simply has nothing to say.
    ///
    /// The receiver is not decided here. It is where the read started rather than where the property
    /// was found, which is how one getter on a prototype answers for every object below it, and the
    /// caller is the one holding that object.
    #[cold]
    #[inline(never)]
    fn getter_of(&self, pair: Value) -> Found {
        let cage = self.isolate.cage();
        let getter = pair
            .to_slot(cage)
            .and_then(AccessorPairRef::from_slot)
            .and_then(|pair| pair.getter(cage));
        match getter {
            Some(getter) => Found::Getter(Value::from_slot(getter, cage)),
            None => Found::Value(Value::UNDEFINED),
        }
    }

    /// Keep looking above an object for a name that is not on it.
    ///
    /// Out of line, because it is the uncommon half of [`Interpreter::property`] and inlining it
    /// would put the loop back in the middle of the opcode that the peeled first step exists to keep
    /// straight. An object with nothing above it reaches the `None` on the first test, which is what
    /// a read of a missing own property costs: one load of the shape's prototype word.
    ///
    /// Nowhere on the chain is `undefined` rather than an error, so this cannot fail and does not
    /// return a `Result`. The one property read that does throw is a read off `undefined` or `null`,
    /// and that is answered by the caller before there is an object to start walking from.
    #[inline(never)]
    fn inherited(&self, object: ObjectRef, name: StringRef) -> Found {
        let cage = self.isolate.cage();
        let mut holder = object.prototype(cage);
        while let Some(current) = holder {
            if let Some((index, attributes)) = current.find(cage, name) {
                let bits = current
                    .value_at(cage, index)
                    .expect("a property the shape has is a property the object has room for");
                return self.found(bits, attributes);
            }
            holder = current.prototype(cage);
        }
        Found::Value(Value::UNDEFINED)
    }

    /// What `this` is in the frame that is running.
    ///
    /// A method call already answered this by putting the object it read the method from on the
    /// frame, and that is the case worth being fast. Everything else is a question the call site
    /// could not answer, because the answer depends on where the code is and whether it is strict,
    /// and both of those are properties of the callee rather than of the caller.
    ///
    /// The three remaining cases are not one rule with exceptions, they are three different rules.
    /// A strict function called with no receiver gets `undefined`, and that one is answerable here
    /// and answered. A sloppy function called with no receiver gets `globalThis`, which needs a real
    /// global object and there is not one yet, so it refuses by name. The outermost frame in a
    /// CommonJS file gets `module.exports`, which is an empty object rather than the global one, and
    /// that needs a module system, so it refuses by name too. Refusing is better than answering
    /// `undefined` in both, because `undefined` is a legal answer that a program will act on and a
    /// refusal is not.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Unsupported`] for the two cases above.
    fn receiver(&self, blueprint: &FunctionBlueprint) -> Result<Value, RuntimeError> {
        let frame = self.stack.current().expect("an opcode runs inside a frame");
        if !frame.receiver.is_empty() {
            return Ok(frame.receiver);
        }
        if self.stack.depth() == 1 {
            return Err(RuntimeError::Unsupported(
                "`this` at the top level is module.exports, and modules are not implemented yet"
                    .to_owned(),
            ));
        }
        if blueprint.strict {
            return Ok(Value::UNDEFINED);
        }
        Err(RuntimeError::Unsupported(
            "`this` in a sloppy mode function called on nothing is globalThis, and globalThis is \
             not implemented yet"
                .to_owned(),
        ))
    }

    /// Write one property, which is an ordinary assignment and not a definition.
    ///
    /// A [`Wrote::Refused`] or a [`Wrote::NoSetter`] means the language would not take the write
    /// rather than that anything went wrong. The caller turns either into silence or into a
    /// `TypeError` depending on whether the code is strict, which is the one place in the language
    /// where the two modes disagree about a store that reached a real object.
    ///
    /// Whether the write is allowed is decided by the first copy of the name on the prototype chain
    /// and not by the object it started from, which is the part that is easy to get wrong. A read
    /// only property on a prototype makes an assignment to every object below it fail, even though
    /// none of those objects has the property, because the chain is searched before the write and
    /// what is found there decides the answer. A setter found on the chain is called instead, on the
    /// object the write started from rather than on the one it was found on, and no own property is
    /// added. Only when the search finds nothing, or finds a plain writable property, does the write
    /// land on the object itself.
    ///
    /// A name that is nowhere on the chain is allowed and adds an own property, which is what an
    /// object literal built by assignment does on every line.
    ///
    /// The object's own properties are searched first and separately, and the write goes straight to
    /// the index that search returned rather than looking the name up a second time. That is the
    /// common case by a long way, it is the one `prop_store` measures, and doing it this way is what
    /// keeps the cost of obeying attributes down to the attribute check itself.
    fn assign(
        &mut self,
        target: ObjectRef,
        name: StringRef,
        value: Value,
    ) -> Result<Wrote, RuntimeError> {
        if let Some((index, attributes)) = target.find(self.isolate.cage(), name) {
            if attributes.is_accessor() {
                let bits = target
                    .value_at(self.isolate.cage(), index)
                    .expect("a property the shape has is a property the object has room for");
                return Ok(self.setter_of(Value::from_bits(bits)));
            }
            if !attributes.is_writable() {
                return Ok(Wrote::Refused);
            }
            target.write_at(self.isolate.heap_mut(), index, value.to_bits());
            return Ok(Wrote::Yes);
        }
        match self.settable_above(target, name) {
            Wrote::Yes => {}
            other => return Ok(other),
        }
        target
            .set(self.isolate.heap_mut(), name, value.to_bits())
            .ok_or(RuntimeError::OutOfMemory)?;
        Ok(Wrote::Yes)
    }

    /// The setter half of a pair, or a refusal when the property has no setter.
    ///
    /// A property with only a getter refuses the write. It is silent in sloppy mode like any other
    /// refusal and it says something different from a read only property in strict mode, which is why
    /// it is its own answer rather than the ordinary refusal.
    #[cold]
    #[inline(never)]
    fn setter_of(&self, pair: Value) -> Wrote {
        let cage = self.isolate.cage();
        let setter = pair
            .to_slot(cage)
            .and_then(AccessorPairRef::from_slot)
            .and_then(|pair| pair.setter(cage));
        match setter {
            Some(setter) => Wrote::Setter(Value::from_slot(setter, cage)),
            None => Wrote::NoSetter,
        }
    }

    /// What the prototype chain above `object` says about adding a property of this name.
    ///
    /// [`Wrote::Yes`] when the name is nowhere above, which is the ordinary answer, and when the
    /// first copy of it found is a writable data property. A read only property above refuses the
    /// write even though the object being written to does not have the property at all, and a setter
    /// above is called instead of the property being added, which is the mechanism that lets one
    /// setter on a prototype receive the writes of every object under it.
    ///
    /// Out of line, because a store to a name the object already has never reaches it.
    #[inline(never)]
    fn settable_above(&self, object: ObjectRef, name: StringRef) -> Wrote {
        let cage = self.isolate.cage();
        let mut holder = object.prototype(cage);
        while let Some(current) = holder {
            if let Some((index, attributes)) = current.find(cage, name) {
                if attributes.is_accessor() {
                    let bits = current
                        .value_at(cage, index)
                        .expect("a property the shape has is a property the object has room for");
                    return self.setter_of(Value::from_bits(bits));
                }
                return if attributes.is_writable() {
                    Wrote::Yes
                } else {
                    Wrote::Refused
                };
            }
            holder = current.prototype(cage);
        }
        Wrote::Yes
    }

    /// What a strict mode assignment to a property that will not take one says.
    ///
    /// Node names the object as well as the property, and it names it two different ways: an
    /// ordinary object is `#<Object>` and one that inherits from nothing is `[object Object]`. Both
    /// were measured rather than guessed, and the rule that tells them apart is the same one that
    /// decides whether an object can be converted to text at all, which is whether the chain reaches
    /// this realm's `Object.prototype`.
    ///
    /// Out of line and cold, for the reason [`Interpreter::nothing_to_read`] is.
    #[cold]
    #[inline(never)]
    fn read_only(
        &self,
        target: ObjectRef,
        blueprint: &FunctionBlueprint,
        key: katsu_ir::ConstIndex,
    ) -> RuntimeError {
        let key = Self::constant_name(blueprint, key);
        let what = if self.inherits_object_prototype(target) {
            "#<Object>"
        } else {
            "[object Object]"
        };
        RuntimeError::Type(format!(
            "Cannot assign to read only property '{key}' of object '{what}'"
        ))
    }

    /// What a strict mode assignment to a getter with no setter says.
    ///
    /// A different sentence from [`Interpreter::read_only`] and not a variation on it. Node does not
    /// quote the property name here and does quote it there, and it writes `of #<Object>` here
    /// against `of object '#<Object>'` there. Both were measured. The one thing the two share is how
    /// the object is named, which is the same rule about reaching this realm's `Object.prototype`.
    ///
    /// Out of line and cold, for the reason [`Interpreter::read_only`] is.
    #[cold]
    #[inline(never)]
    fn only_a_getter(
        &self,
        target: ObjectRef,
        blueprint: &FunctionBlueprint,
        key: katsu_ir::ConstIndex,
    ) -> RuntimeError {
        let key = Self::constant_name(blueprint, key);
        let what = if self.inherits_object_prototype(target) {
            "#<Object>"
        } else {
            "[object Object]"
        };
        RuntimeError::Type(format!(
            "Cannot set property {key} of {what} which has only a getter"
        ))
    }

    /// What reading a property of `undefined` or `null` says, which is what Node says word for word.
    ///
    /// Out of line and cold, so that the formatting and the allocation it costs are not code sitting
    /// in the middle of an opcode that almost always succeeds.
    #[cold]
    #[inline(never)]
    fn nothing_to_read(
        object: Value,
        blueprint: &FunctionBlueprint,
        key: katsu_ir::ConstIndex,
    ) -> RuntimeError {
        let what = Self::primitive_text(object);
        let key = Self::constant_name(blueprint, key);
        RuntimeError::Type(format!(
            "Cannot read properties of {what} (reading '{key}')"
        ))
    }

    /// What writing a property of `undefined` or `null` says, word for word as Node says it.
    #[cold]
    #[inline(never)]
    fn nothing_to_write(
        object: Value,
        blueprint: &FunctionBlueprint,
        key: katsu_ir::ConstIndex,
    ) -> RuntimeError {
        let what = Self::primitive_text(object);
        let key = Self::constant_name(blueprint, key);
        RuntimeError::Type(format!("Cannot set properties of {what} (setting '{key}')"))
    }

    /// What writing a property of a primitive says in strict mode, word for word as Node says it.
    ///
    /// The value is named as well as its type, because the whole reason strict mode reports this
    /// instead of ignoring it is that the write went somewhere the program did not expect, and the
    /// value is what says which one it was.
    #[cold]
    #[inline(never)]
    fn nowhere_to_write(
        &self,
        object: Value,
        blueprint: &FunctionBlueprint,
        key: katsu_ir::ConstIndex,
    ) -> RuntimeError {
        let kind = self.type_of(object);
        let what = self.display(object);
        let key = Self::constant_name(blueprint, key);
        RuntimeError::Type(format!("Cannot create property '{key}' on {kind} '{what}'"))
    }

    /// The text `console.log` prints for an object.
    ///
    /// Node prints the contents rather than `[object Object]`, because `console.log` does not convert
    /// its argument to a string, it inspects it, and somebody printing an object wants to see what is
    /// in it. Strings inside an object are quoted and a string printed on its own is not, for the same
    /// reason: inside an object the quotes are what tell a string apart from a name.
    ///
    /// The depth is Node's, and it is a limit on how much of a large object is worth looking at
    /// rather than a way of stopping an endless walk. Stopping the walk is the cycle set's job, and
    /// the two are separate because a cycle is reported wherever it is found and a depth limit only
    /// applies below the third level, so an object that points back at itself from four levels down
    /// prints `[Circular *1]` and not `[Object]`.
    fn object_text(
        &self,
        object: ObjectRef,
        depth: u32,
        indent: usize,
        cycles: &mut Cycles,
    ) -> String {
        let slot = object.slot();
        // Asked before the depth check, and that order was measured rather than chosen.
        if cycles.inside(slot) {
            return inspect::circular(cycles.number(slot));
        }
        let cage = self.isolate.cage();
        // What is visible rather than what is there. Node hides non enumerable properties unless it
        // is asked for them, which is what makes it possible to put a method on a prototype without
        // it turning up in the printed form of every object that inherits it.
        let names = object.enumerable(cage);
        // An object that inherits from nothing says so, and the tag counts towards the width the
        // same way a back reference does, so an object with it breaks onto several lines earlier
        // than the same object without it. That was measured against node rather than assumed.
        let tag = if object.prototype(cage).is_none() {
            inspect::NULL_PROTOTYPE
        } else {
            ""
        };
        // Also before the depth check, and also measured. An object with nothing in it prints as
        // `{}` however deep it is, because there is nothing below it for the limit to be protecting
        // anybody from, and `[Object]` would be six characters longer as well as less informative.
        // The differential harness found this one: `[Object]` is long enough to push the object
        // holding it over the width limit, so getting it wrong breaks a line that Node keeps whole.
        if names.is_empty() {
            return inspect::braces(&[], indent, tag);
        }
        if depth == 0 {
            // The tag on its own rather than `[Object]`, because at the depth limit the one thing
            // still worth saying about an object is the thing its contents would not have shown.
            return if tag.is_empty() {
                "[Object]".to_owned()
            } else {
                tag.to_owned()
            };
        }
        cycles.enter(slot);
        let entries: Vec<String> = names
            .into_iter()
            .map(|(name, index, attributes)| {
                let value = object
                    .value_at(cage, index)
                    .expect("a name at this index means there is a value at it");
                let key = inspect::key(&name.to_utf8_lossy(cage));
                // An accessor prints as what it is rather than as what it would say, because
                // printing an object is not supposed to run any of the program's code. Node makes
                // the same choice and prints `[Getter]`, and it is the right one: a getter can throw,
                // can loop, and can change the object being printed underneath the printer.
                if attributes.is_accessor() {
                    let (getter, setter) = self.accessor_halves(Value::from_bits(value));
                    let what = match (getter.is_some(), setter.is_some()) {
                        (true, true) => "[Getter/Setter]",
                        (true, false) => "[Getter]",
                        (false, true) => "[Setter]",
                        // Both halves absent, which `{get: undefined, set: undefined}` legally
                        // makes. Node prints `undefined` for this one, not a fourth bracketed word,
                        // which was measured. It reads as the value a read of the property would
                        // give, and a read of a property with no getter does give `undefined`.
                        (false, false) => "undefined",
                    };
                    return format!("{key}: {what}");
                }
                let value = self.inspect(
                    Value::from_bits(value),
                    depth - 1,
                    inspect::nested(indent),
                    cycles,
                );
                format!("{key}: {value}")
            })
            .collect();
        cycles.leave();
        // Read after the walk and not before it, because whether anything points back at this
        // object is only known once everything under it has been printed.
        let base = match (cycles.assigned(slot), tag) {
            (Some(number), "") => inspect::reference(number),
            (Some(number), tag) => format!("{} {tag}", inspect::reference(number)),
            (None, tag) => tag.to_owned(),
        };
        inspect::braces(&entries, indent, &base)
    }

    /// What one value looks like inside a printed object.
    ///
    /// A string is quoted here and not quoted when it is printed on its own, which is not an
    /// inconsistency: inside an object the quotes are what tell a string apart from a name.
    fn inspect(&self, value: Value, depth: u32, indent: usize, cycles: &mut Cycles) -> String {
        if let Some(string) = self.as_string(value) {
            return inspect::quote(&string.to_utf8_lossy(self.isolate.cage()));
        }
        if let Some(object) = self.as_object(value) {
            return self.object_text(object, depth, indent, cycles);
        }
        self.display(value)
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
    /// This used to be a range check against the cage, on the grounds that every heap object was a
    /// string. Closures and contexts ended that, so it reads the kind word and a value that points
    /// at either of them is not a string. One chokepoint rather than a dozen scattered casts is the
    /// entire reason the function exists, and it is what made adding two object kinds a change to
    /// two functions rather than a hunt through the match.
    fn as_string(&self, value: Value) -> Option<StringRef> {
        let slot = value.to_slot(self.isolate.cage())?;
        match HeapKind::of(self.isolate.cage(), slot)? {
            HeapKind::String => StringRef::from_slot(slot),
            HeapKind::Closure
            | HeapKind::Context
            | HeapKind::Native
            | HeapKind::Object
            | HeapKind::Shape
            | HeapKind::Properties
            | HeapKind::AccessorPair => None,
        }
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
    /// Cold as well as out of line, and the difference between the two is worth twenty five percent
    /// of a counting loop on Windows. `ToBoolean` is what every conditional jump goes through, so a
    /// merely out of line call sits on the hot path of the loop and forces the values the loop is
    /// carrying into callee saved registers whether the call is taken or not. Marked cold, the
    /// spilling moves into the branch that is not taken.
    #[cold]
    #[inline(never)]
    fn heap_truthy(&self, value: Value) -> bool {
        match self.as_string(value) {
            Some(string) => !string.is_empty(self.isolate.cage()),
            None => value.to_boolean(),
        }
    }

    /// The `typeof` operator.
    ///
    /// A pointer answers three different ways depending on what it points at, so this reads the kind
    /// word once rather than asking `is_string` and then `as_closure` and paying for two. Everything
    /// that is not a pointer is decided by the tag and never touches the cage.
    ///
    /// A context is never in a register, because nothing in the instruction set moves one there, so
    /// the arm for it is a fallback rather than a case that happens.
    ///
    /// The pointer test comes first and is a tag check, because `to_slot` narrows a small integer
    /// into a slot as happily as it narrows an address, and a small integer is a number rather than
    /// something to go looking for a kind word in front of.
    fn type_of(&self, value: Value) -> &'static str {
        if !value.is_pointer() {
            return value.type_of();
        }
        let kind = value
            .to_slot(self.isolate.cage())
            .and_then(|slot| HeapKind::of(self.isolate.cage(), slot));
        match kind {
            Some(HeapKind::String) => "string",
            Some(HeapKind::Closure | HeapKind::Native) => "function",
            Some(
                HeapKind::Object
                | HeapKind::Context
                | HeapKind::Shape
                | HeapKind::Properties
                | HeapKind::AccessorPair,
            )
            | None => "object",
        }
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
    /// A function converts to the text `console.log` shows for it, which is not what Node does.
    /// Node hands back the source text the function was written with, because that is what
    /// `Function.prototype.toString` is specified to return, and reproducing it means keeping the
    /// span of every function and the source text to slice it out of. That is its own piece of work,
    /// it is the same piece of work that makes `x is not a function` name `x` instead of naming the
    /// value, and until it lands this at least says the thing is a function.
    fn coerce_to_string(&mut self, value: Value) -> Result<StringRef, RuntimeError> {
        // A value that is already a string hands back the reference it already has rather than a
        // copy of it, which is why this cannot simply call `text_of` and allocate the answer.
        if let Some(string) = self.as_string(value) {
            return Ok(string);
        }
        let text = self.text_of(value)?;
        self.isolate
            .allocate_string(&text)
            .ok_or(RuntimeError::OutOfMemory)
    }

    /// The rules of `ToString`, as Rust text and without touching the heap.
    ///
    /// Split out of [`Interpreter::coerce_to_string`] so that `String(x)` and `'' + x` cannot come
    /// to disagree. They are the same conversion in the language and a second copy of these rules
    /// would eventually be a second answer.
    fn text_of(&self, value: Value) -> Result<String, RuntimeError> {
        if let Some(string) = self.as_string(value) {
            return Ok(string.to_utf8_lossy(self.isolate.cage()).into_owned());
        }
        if let Some(closure) = self.as_closure(value) {
            return Ok(self.function_text(closure));
        }
        if let Some(native) = self.as_native(value) {
            return Ok(self.native_text(native));
        }
        if let Some(object) = self.as_object(value) {
            return self.object_to_text(object);
        }
        Ok(Self::primitive_text(value))
    }

    /// `ToString` of an object, which is where the prototype chain first becomes observable.
    ///
    /// An object has no text of its own. What converts it is the `toString` it inherits, and an
    /// object that inherits from nothing has none, so `String(Object.create(null))` is a `TypeError`
    /// in Node and here, in the same words. That is not an edge case dressed up: it is the reason
    /// `[object Object]` is the answer for an ordinary object, because that text comes from
    /// `Object.prototype.toString` rather than from the object.
    ///
    /// The test is whether the chain reaches this realm's `Object.prototype`, and not whether a
    /// property called `toString` is found on it. There is a real one on it now, and it answers
    /// exactly this text, so the two questions still have the same answer for every object this
    /// build can make. What stops it being a real lookup and a real call is that an object with its
    /// own `toString` would then have it called, and calling a JavaScript function from Rust is not
    /// something the interpreter can do yet. Until it can, an overridden `toString` is ignored by
    /// `String(x)` and `'' + x`, which is a known wrong answer and the next thing this becomes.
    ///
    /// An object converts to `[object Object]` and not to what `console.log` shows, because this is
    /// `ToString` and not inspection. `'' + {}` is `[object Object]` in Node too, and the two
    /// differing is a real distinction in the language rather than an inconsistency.
    fn object_to_text(&self, object: ObjectRef) -> Result<String, RuntimeError> {
        if self.inherits_object_prototype(object) {
            return Ok("[object Object]".to_owned());
        }
        Err(RuntimeError::Type(
            "Cannot convert object to primitive value".to_owned(),
        ))
    }

    /// Whether an object inherits from this realm's `Object.prototype`.
    ///
    /// Two unrelated questions turn out to be this one. Whether an object can be converted to text
    /// is whether it can reach a `toString`, and how Node names an object in a `TypeError` is
    /// `#<Object>` for one that inherits and `[object Object]` for one that does not. Both are
    /// really asking whether the thing is an ordinary object or one built by `Object.create(null)`,
    /// so they ask it in one place.
    fn inherits_object_prototype(&self, object: ObjectRef) -> bool {
        let cage = self.isolate.cage();
        let top = self.isolate.object_prototype_if_built();
        let mut holder = Some(object);
        while let Some(current) = holder {
            if Some(current) == top {
                return true;
            }
            holder = current.prototype(cage);
        }
        false
    }

    /// The text of a primitive, meaning everything that is not on the heap.
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
        // Everything else on the heap. An ordinary object never gets here, because `text_of` sends
        // one to `object_to_text` first, and this is the answer for a context or anything else that
        // is on the heap without being a value a program can hold.
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
        // ToPrimitive on both sides first and then the string test, which is the order the
        // specification gives and is not the same as testing for a string and converting after.
        // `{} + 1` is the case that tells them apart: neither side is a string, and the answer is
        // still the text `[object Object]1` rather than NaN, because the object became a string on
        // the way in.
        let left = self.coerce_to_primitive(left)?;
        let right = self.coerce_to_primitive(right)?;
        if self.is_string(left) || self.is_string(right) {
            return self.concatenate(left, right);
        }
        Ok(Value::from_f64(self.number(left) + self.number(right)))
    }

    /// `ToPrimitive`, which today gives back a string or the value it was handed.
    ///
    /// Every heap value that is not already a string reaches the default `toString`, because there
    /// is nothing on `Object.prototype` yet to carry a `valueOf` and no `Symbol.toPrimitive` to look
    /// up first. That is why the hint the specification passes is not threaded through here: number
    /// and string and default all reach the same place, so a parameter for it would be a parameter
    /// nothing reads. The hint arrives with the methods, and so does the ability of this to run code
    /// and throw, which is why it already returns a `Result`. It throws today for one reason only,
    /// which is an object whose chain reaches nothing at all and so has no conversion to reach.
    ///
    /// The test for having anything to do is `is_pointer`, which is a range check on a word that is
    /// already in a register rather than a load of a heap kind, so a comparison between two numbers
    /// or two strings pays a predictable branch and nothing else.
    fn coerce_to_primitive(&mut self, value: Value) -> Result<Value, RuntimeError> {
        if !value.is_pointer() || self.is_string(value) {
            return Ok(value);
        }
        let string = self.coerce_to_string(value)?;
        Ok(self.string_value(string))
    }

    /// Whether a value is an object rather than a primitive, which for `==` is the whole question.
    ///
    /// A string is on the heap and is not an object, which is the one case that makes this more than
    /// a pointer test.
    fn is_object(&self, value: Value) -> bool {
        value.is_pointer() && !self.is_string(value)
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
    fn loose_equal(&mut self, left: Value, right: Value) -> Result<bool, RuntimeError> {
        if let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) {
            return Ok(left == right);
        }
        self.loose_equal_slowly(left, right)
    }

    /// `==` when at least one side is not a number.
    #[cold]
    #[inline(never)]
    fn loose_equal_slowly(&mut self, left: Value, right: Value) -> Result<bool, RuntimeError> {
        // Checked before anything else, because `null == undefined` is true and neither of them is
        // equal to anything else at all, not to zero, not to false, not to the empty string and not
        // to an object.
        if left.is_nullish() || right.is_nullish() {
            return Ok(left.is_nullish() && right.is_nullish());
        }
        // Two of a kind is the strict question, which covers both numbers, both strings, both
        // booleans and both objects. Objects belong in that list rather than in the conversion below
        // it, because two objects are equal when they are the same object and never because their
        // contents match. Left out, an object compared against itself would fall all the way through
        // to the number comparison and answer false, since its primitive is a string that is NaN as
        // a number and NaN is not equal to itself.
        if left.is_number() && right.is_number()
            || left.is_bool() && right.is_bool()
            || self.is_string(left) && self.is_string(right)
            || self.is_object(left) && self.is_object(right)
        {
            return Ok(self.strict_equal(left, right));
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
        // An object against a primitive converts the object and asks again, the same recursion the
        // boolean case above uses. It is why `({}) == '[object Object]'` is true, which reads like a
        // curiosity and is the mechanism `[] == ''` and `[1] == 1` rest on once there are arrays.
        if self.is_object(left) {
            let left = self.coerce_to_primitive(left)?;
            return self.loose_equal(left, right);
        }
        if self.is_object(right) {
            let right = self.coerce_to_primitive(right)?;
            return self.loose_equal(left, right);
        }
        // The only pair left is a number against a string, and the string is the side that converts,
        // which is why `"0x10" == 16` is true.
        Ok(self.number(left) == self.number(right))
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
    /// The specification also carries a flag saying which operand to convert first, because `<=` and
    /// `>` swap their operands and still have to convert the one written on the left first. Nothing
    /// can tell the difference yet, since no conversion here can run code or throw, so the four arms
    /// go on passing their operands in whichever order makes the comparison right and the flag
    /// arrives with `valueOf`.
    ///
    /// Two numbers are the case every loop condition in every program is, so they are tested for
    /// first and answered without touching the heap, for the same reason the addition above is.
    fn less_than(&mut self, left: Value, right: Value) -> Result<Option<bool>, RuntimeError> {
        match (left.as_f64(), right.as_f64()) {
            (Some(left), Some(right)) => Ok(compare_numbers(left, right)),
            _ => self.less_than_slowly(left, right),
        }
    }

    /// The relational comparison when at least one side is not already a number.
    ///
    /// The conversion runs before the string test rather than after it, which is what makes
    /// `'9' < {}` true: an object is not a string, but its primitive is, so this ends up comparing
    /// `'9'` against `'[object Object]'` by code unit rather than comparing two NaNs.
    #[cold]
    #[inline(never)]
    fn less_than_slowly(
        &mut self,
        left: Value,
        right: Value,
    ) -> Result<Option<bool>, RuntimeError> {
        let left = self.coerce_to_primitive(left)?;
        let right = self.coerce_to_primitive(right)?;
        if let (Some(left), Some(right)) = (self.as_string(left), self.as_string(right)) {
            return Ok(Some(
                left.compare(self.isolate.cage(), right) == Sorting::Less,
            ));
        }
        Ok(compare_numbers(self.number(left), self.number(right)))
    }
}

/// What one printed value's walk remembers about the objects it is inside.
///
/// Two separate things, and keeping them apart is what makes the output match Node. The path is the
/// objects between the root and wherever the walk is now, and it is an ancestor test rather than a
/// seen test: printing `{ p: e, q: e }` shows `e` twice with no reference on either, because the
/// second `e` is not inside the first. The numbers are the objects a walk did find its way back
/// into, and a number is handed out where the way back is found rather than where the object was
/// first printed, which is why the first cycle in reading order is always the one numbered one.
///
/// Both are vectors rather than sets, because a cycle is rare and a printed object has a handful of
/// levels above it, so a linear scan over a slice that is already in cache beats hashing.
#[derive(Default)]
struct Cycles {
    path: Vec<katsu_gc::Slot>,
    numbers: Vec<katsu_gc::Slot>,
}

impl Cycles {
    /// Whether the walk is already inside this object, which is what makes a reference a cycle.
    fn inside(&self, slot: katsu_gc::Slot) -> bool {
        self.path.contains(&slot)
    }

    /// The number for an object the walk has just found its way back into, handing out a new one the
    /// first time and the same one every time after.
    fn number(&mut self, slot: katsu_gc::Slot) -> usize {
        if let Some(at) = self.numbers.iter().position(|held| *held == slot) {
            return at + 1;
        }
        self.numbers.push(slot);
        self.numbers.len()
    }

    /// The number this object was given, if anything under it pointed back at it.
    fn assigned(&self, slot: katsu_gc::Slot) -> Option<usize> {
        self.numbers
            .iter()
            .position(|held| *held == slot)
            .map(|at| at + 1)
    }

    fn enter(&mut self, slot: katsu_gc::Slot) {
        self.path.push(slot);
    }

    fn leave(&mut self) {
        self.path.pop();
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
        CacheIndex, CodeOffset, ConstIndex, ConstantPool, FunctionBlueprint, Handler, Op, Register,
    };

    use std::fmt::Write;

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

    /// Assemble instructions that protect some of themselves, and verify the lot.
    ///
    /// Written out by hand rather than lowered from source, because the point of these tests is
    /// what the loop does with a table and not what the frontend puts in one. A table lowering
    /// cannot produce today, such as two entries over the same range, is exactly the case worth
    /// asking about.
    fn assemble_handled(code: Vec<Op>, handlers: Vec<Handler>) -> FunctionBlueprint {
        let blueprint = FunctionBlueprint {
            frame_size: FRAME,
            cache_slots: 1,
            code,
            handlers,
            ..FunctionBlueprint::default()
        };
        blueprint.verify().expect("the test assembled bad bytecode");
        blueprint
    }

    /// One entry, spelled with plain numbers so a test reads as a table rather than as a struct.
    fn handler(start: u32, end: u32, target: u32, register: u16) -> Handler {
        Handler {
            start: CodeOffset(start),
            end: CodeOffset(end),
            target: CodeOffset(target),
            register: Register(register),
        }
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
    fn a_thrown_value_that_nothing_catches_comes_back_as_the_text_of_that_value() {
        // A value is an address in one interpreter's heap, so it cannot leave as itself. This is
        // where it stops being a value, and it is the last point at which anything can read it.
        let mut constants = ConstantPool::default();
        let boom = constants.string("boom");
        assert_eq!(
            run_with(
                vec![
                    Op::LoadConst {
                        dst: Register(0),
                        src: boom,
                    },
                    Op::Throw { src: Register(0) },
                ],
                constants,
            ),
            Err(RuntimeError::Uncaught("boom".to_owned()))
        );
    }

    #[test]
    fn a_throw_inside_a_protected_range_lands_in_the_handler_with_the_value() {
        let blueprint = assemble_handled(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 7,
                },
                Op::Throw { src: Register(0) },
                // Never runs, and it is here so that a handler that did not take would return
                // something visibly different rather than the same answer by luck.
                Op::LoadInt {
                    dst: Register(1),
                    value: 99,
                },
                Op::Return { src: Register(1) },
                Op::Return { src: Register(2) },
            ],
            vec![handler(0, 2, 4, 2)],
        );
        assert_eq!(
            Interpreter::new()
                .expect("should reserve a stack")
                .run(&blueprint),
            Ok(Value::from_i32(7))
        );
    }

    #[test]
    fn a_throw_outside_every_range_is_not_caught() {
        // The same code with the range moved off the throw. This is the test that says the search
        // reads the table rather than taking the first entry it finds.
        let blueprint = assemble_handled(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 7,
                },
                Op::Throw { src: Register(0) },
                Op::Return { src: Register(2) },
            ],
            vec![handler(2, 3, 2, 2)],
        );
        assert_eq!(
            Interpreter::new()
                .expect("should reserve a stack")
                .run(&blueprint),
            Err(RuntimeError::Uncaught("7".to_owned()))
        );
    }

    #[test]
    fn the_first_entry_whose_range_contains_the_throw_wins() {
        // Two entries over exactly the same range, which is a table lowering would not build and
        // is the shortest way to ask whether order decides. It has to, because order is how a
        // nested `try` is told from the one around it without comparing ranges.
        let blueprint = assemble_handled(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 1,
                },
                Op::Throw { src: Register(0) },
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
            ],
            vec![handler(0, 2, 2, 3), handler(0, 2, 4, 3)],
        );
        assert_eq!(
            Interpreter::new()
                .expect("should reserve a stack")
                .run(&blueprint),
            Ok(Value::from_i32(10))
        );
    }

    #[test]
    fn an_engine_error_becomes_an_object_the_moment_something_catches_it() {
        // Until here it is a name and a message, which is what makes entering a `try` free and an
        // uncaught error cheap. Not an `Error` instance, because there is no `Error` to be an
        // instance of yet: that needs an `Error` constructor, and so does the `stack` it lacks.
        let blueprint = assemble_handled(
            vec![Op::ThrowConstAssignment, Op::Return { src: Register(0) }],
            vec![handler(0, 1, 1, 0)],
        );
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let value = interpreter.run(&blueprint).expect("the handler catches it");
        assert_eq!(
            interpreter.display(value),
            "{ name: 'TypeError', message: 'Assignment to constant variable.' }"
        );
    }

    #[test]
    fn what_no_program_could_have_caused_is_not_catchable() {
        // An opcode this build does not run is a gap in katsu rather than an event in the program,
        // and a `catch` that swallowed one would turn a missing feature into a wrong answer. Node
        // draws the same line: a program cannot catch running out of memory either.
        let blueprint = assemble_handled(
            vec![
                Op::GetIndex {
                    dst: Register(0),
                    obj: Register(1),
                    index: Register(2),
                    cache: IC,
                },
                Op::Return { src: Register(0) },
            ],
            vec![handler(0, 1, 1, 0)],
        );
        assert!(matches!(
            Interpreter::new()
                .expect("should reserve a stack")
                .run(&blueprint),
            Err(RuntimeError::NotImplemented(_))
        ));
    }

    #[test]
    fn an_interrupt_is_not_catchable_either() {
        // Otherwise a program could refuse to be stopped by wrapping itself in a `try`, which is
        // the one thing the interrupt exists to prevent.
        let blueprint = assemble_handled(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 0,
                },
                Op::LoopBackEdge {
                    target: CodeOffset(0),
                    profile: IC,
                },
                Op::Return { src: Register(0) },
            ],
            vec![handler(0, 2, 2, 0)],
        );
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let interrupt = interpreter.interrupt();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            interrupt.request();
        });
        assert_eq!(interpreter.run(&blueprint), Err(RuntimeError::Interrupted));
    }

    #[test]
    fn catching_leaves_the_stack_where_it_was() {
        // The handler runs in the frame that owns it, so nothing is left over and the interpreter
        // is as usable afterwards as it is after a return.
        let blueprint = assemble_handled(
            vec![
                Op::LoadInt {
                    dst: Register(0),
                    value: 5,
                },
                Op::Throw { src: Register(0) },
                Op::Return { src: Register(1) },
            ],
            vec![handler(0, 2, 2, 1)],
        );
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        assert_eq!(interpreter.run(&blueprint), Ok(Value::from_i32(5)));
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

    /// Compile a program so that its last expression is the value it returns.
    ///
    /// Lowering does not compute script completion values yet. The top level ends by loading
    /// `undefined` over the register the last expression went into and returning that register, so
    /// turning that one load into a self move leaves the expression's value exactly where the return
    /// reads it. Replacing rather than deleting keeps every instruction at the offset it already
    /// had, which matters because a jump can target the instruction being replaced: `if (c) { f(); }`
    /// jumps to it when the condition is false.
    ///
    /// The tests above assemble bytecode because they are about one opcode each. The tests below go
    /// through the frontend because a call is the one thing where the interesting behaviour is the
    /// agreement between lowering, scope analysis and the loop, and hand written bytecode would be
    /// testing the loop against my idea of what lowering emits rather than against what it emits.
    fn as_expression(source: &str) -> FunctionBlueprint {
        let mut blueprint = crate::compile("t.js", source).expect("should compile");
        let at = blueprint.code.len() - 2;
        let Op::LoadUndefined { dst } = blueprint.code[at] else {
            panic!("a program used here should end with an expression statement");
        };
        blueprint.code[at] = Op::Move { dst, src: dst };
        blueprint
            .verify()
            .expect("replacing one op kept it well formed");
        blueprint
    }

    /// Run a program and hand back the value of its last expression.
    fn evaluate(source: &str) -> Result<Value, RuntimeError> {
        Interpreter::new()
            .expect("should reserve a stack")
            .run(&as_expression(source))
    }

    #[track_caller]
    fn evaluate_number(source: &str) -> f64 {
        evaluate(source)
            .expect("should not throw")
            .as_f64()
            .expect("should produce a number")
    }

    /// Run a program and print what its last expression produced, the way `console.log` would.
    #[track_caller]
    fn evaluate_display(source: &str) -> String {
        let blueprint = as_expression(source);
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let value = interpreter.run(&blueprint).expect("should not throw");
        interpreter.display(value)
    }

    #[test]
    fn a_call_runs_the_callee_and_comes_back_with_what_it_returned() {
        assert_eq!(
            evaluate_number("function twice(x) { return x * 2; } twice(21);"),
            42.0
        );
    }

    #[test]
    fn a_call_returns_undefined_when_the_body_falls_off_the_end() {
        assert_eq!(
            evaluate("function nothing() {} nothing();"),
            Ok(Value::UNDEFINED)
        );
    }

    #[test]
    fn a_method_call_makes_this_the_object_it_was_read_from() {
        assert_eq!(
            evaluate_number("var o = { n: 41, get: function () { return this.n + 1; } }; o.get();"),
            42.0
        );
    }

    #[test]
    fn this_is_the_object_the_call_went_through_and_not_the_one_the_function_came_from() {
        // The property that makes a receiver a receiver rather than a second closure variable. The
        // same function reached through two objects reads two different values.
        assert_eq!(
            evaluate_display(
                "var one = { n: 1, read: function () { return this.n; } };\n\
                 var two = { n: 2, read: one.read };\n\
                 one.read() + ',' + two.read();"
            ),
            "1,2"
        );
    }

    #[test]
    fn a_method_can_write_through_its_receiver() {
        assert_eq!(
            evaluate_number(
                "var c = { n: 0, bump: function () { this.n = this.n + 1; return this.n; } };\n\
                 c.bump(); c.bump(); c.n;"
            ),
            2.0
        );
    }

    #[test]
    fn a_strict_function_called_on_nothing_gets_undefined() {
        assert_eq!(
            evaluate("function f() { 'use strict'; return this; } f();"),
            Ok(Value::UNDEFINED)
        );
    }

    #[test]
    fn a_sloppy_function_called_on_nothing_refuses_by_name_rather_than_guessing() {
        // `globalThis`, and there is no global object yet. Answering `undefined` would be a legal
        // value that a program would go on to read a property from, and the failure would then be
        // reported somewhere else entirely.
        let error = evaluate("function f() { return this; } f();").expect_err("should refuse");
        assert!(
            matches!(&error, RuntimeError::Unsupported(text) if text.contains("globalThis")),
            "{error:?}"
        );
    }

    #[test]
    fn this_at_the_top_level_refuses_by_name_because_it_is_module_exports() {
        let error = evaluate("this;").expect_err("should refuse");
        assert!(
            matches!(&error, RuntimeError::Unsupported(text) if text.contains("module.exports")),
            "{error:?}"
        );
    }

    #[test]
    fn a_call_leaves_the_callers_registers_alone() {
        // The two frames are neighbouring windows into one region, so a callee that sized its frame
        // wrong or copied its arguments to the wrong place writes over the caller's variables. The
        // three bindings are read after the call rather than before it for that reason.
        assert_eq!(
            evaluate_number(
                "function twice(x) { return x * 2; } \
                 function main() { const a = 10; const b = twice(3); const c = 100; \
                 return a + b + c; } main();"
            ),
            116.0
        );
    }

    #[test]
    fn a_function_can_call_itself() {
        // The whole mechanism at once: a self reference read out of the environment, two calls in
        // one frame, and returns that have to land in the right register of the right frame.
        assert_eq!(
            evaluate_number(
                "function fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); } fib(20);"
            ),
            6765.0
        );
    }

    #[test]
    fn a_deep_recursion_returns_all_the_way_back() {
        assert_eq!(
            evaluate_number(
                "function down(n) { if (n === 0) return 0; return 1 + down(n - 1); } down(1000);"
            ),
            1000.0
        );
    }

    #[test]
    fn a_nested_function_reads_a_variable_from_the_scope_it_was_written_in() {
        assert_eq!(
            evaluate_number(
                "function outer() { let a = 3; function inner() { return a + 1; } \
                 return inner(); } outer();"
            ),
            4.0
        );
    }

    #[test]
    fn a_closure_still_reads_its_variable_after_the_call_that_made_it_returned() {
        // The frame `counter` ran in is gone by the time `next` runs, so `n` was never living in it.
        // Three calls rather than one, because a closure that captured a copy answers 1 every time.
        assert_eq!(
            evaluate_number(
                "function counter() { let n = 0; function next() { n = n + 1; return n; } \
                 return next; } const step = counter(); step(); step(); step();"
            ),
            3.0
        );
    }

    #[test]
    fn two_closures_over_one_function_get_a_variable_each() {
        assert_eq!(
            evaluate_number(
                "function counter() { let n = 0; function next() { n = n + 1; return n; } \
                 return next; } const a = counter(); const b = counter(); a(); a(); b();"
            ),
            1.0
        );
    }

    #[test]
    fn a_variable_two_levels_out_is_two_hops_when_the_level_between_captured_something() {
        assert_eq!(
            evaluate_number(
                "function a() { let v = 1; function b() { let w = 20; \
                 function c() { return v + w; } return c(); } return b(); } a();"
            ),
            21.0
        );
    }

    #[test]
    fn a_level_that_captured_nothing_does_not_cost_a_hop() {
        // The same nesting with the middle function holding nothing of its own, so `c` finds `v`
        // without walking. Scope analysis only asks for a context where something is captured, which
        // is what keeps the walk proportional to capturing rather than to how deeply somebody
        // indented.
        assert_eq!(
            evaluate_number(
                "function a() { let v = 7; function b() { function c() { return v; } \
                 return c(); } return b(); } a();"
            ),
            7.0
        );
    }

    #[test]
    fn arguments_past_the_declared_parameters_are_dropped() {
        // The registers above the parameters are the callee's scratch space, so an extra argument
        // copied into one of them is a value sitting where the callee expects to find its own.
        assert_eq!(
            evaluate_number("function one(a) { return a; } one(1, 2, 3);"),
            1.0
        );
    }

    #[test]
    fn a_parameter_that_was_not_passed_is_undefined() {
        assert_eq!(
            evaluate("function two(a, b) { return b; } two(1);"),
            Ok(Value::UNDEFINED)
        );
    }

    #[test]
    fn a_string_literal_inside_a_function_body_is_interned_like_any_other() {
        // Unreachable until there was a way into a function body, which is why the pass that
        // resolves constants had to grow from the top level to every function in the unit.
        assert_eq!(
            evaluate_display("function greet() { return 'hello'; } greet();"),
            "hello"
        );
    }

    #[test]
    fn calling_something_that_is_not_a_function_says_so_and_says_what_it_was() {
        assert_eq!(
            evaluate("const x = 5; x();"),
            Err(RuntimeError::Type("5 is not a function".to_owned()))
        );
    }

    #[test]
    fn a_function_that_never_stops_calling_itself_reports_the_stack_rather_than_crashing() {
        // The message is Node's, word for word, because a program that prints what it caught should
        // not be able to tell which engine caught it.
        assert_eq!(
            evaluate("function f() { return f(); } f();"),
            Err(RuntimeError::Range(
                "Maximum call stack size exceeded".to_owned()
            ))
        );
    }

    #[test]
    fn an_interpreter_that_overflowed_its_stack_can_run_the_next_program() {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        let overflow = as_expression("function f() { return f(); } f();");
        assert!(interpreter.run(&overflow).is_err());
        assert_eq!(interpreter.depth(), 0, "the dead frames should be gone");
        let again = interpreter
            .run(&as_expression(
                "function twice(x) { return x * 2; } twice(4);",
            ))
            .expect("should not throw");
        assert_eq!(again.as_f64(), Some(8.0));
    }

    #[test]
    fn the_console_tells_the_two_zeroes_apart_and_string_coercion_does_not() {
        // Found by the differential harness on its first run against node, which is the entire
        // argument for having one. Both halves are asserted together because fixing the first by
        // changing `ToString` would break the second, and the second is the one the specification
        // is explicit about.
        assert_eq!(evaluate_display("-0;"), "-0");
        assert_eq!(evaluate_display("'' + -0;"), "0");
        assert_eq!(evaluate_display("0;"), "0");
        assert_eq!(evaluate_display("0 * -1;"), "-0");
    }

    #[test]
    fn typeof_a_function_is_function_and_not_object() {
        // A closure is a pointer and so is a string, so this only works because the kind word is
        // read. It answered "string" for every pointer before there was more than one kind.
        assert_eq!(
            evaluate_display("function f() { return 1; } typeof f;"),
            "function"
        );
    }

    #[test]
    fn a_function_prints_as_its_name() {
        assert_eq!(
            evaluate_display(
                "function outer() { function greet() { return 1; } return greet; } outer();"
            ),
            "[Function: greet]"
        );
    }

    #[test]
    fn a_function_written_without_a_name_says_so_rather_than_printing_an_empty_one() {
        assert_eq!(
            evaluate_display("function outer() { return function () { return 1; }; } outer();"),
            "[Function (anonymous)]"
        );
    }

    #[test]
    fn a_function_joined_to_a_string_says_that_it_is_a_function() {
        // Node produces the source text the function was written with, which needs spans that are
        // not kept yet. Pinned here so the difference is a decision on the record rather than a
        // surprise the first time a program concatenates one.
        assert_eq!(
            evaluate_display(
                "function outer() { function greet() {} return 'a ' + greet; } outer();"
            ),
            "a [Function: greet]"
        );
    }

    /// A native that adds up whatever it was passed.
    ///
    /// Variadic on purpose, so that one function can show that the arguments arrive, that they
    /// arrive in order, and that a call passing none of them is a call and not a crash.
    ///
    /// The `Result` is the signature rather than something this one uses, which is true of plenty of
    /// real builtins as well.
    #[allow(clippy::unnecessary_wraps)]
    fn sum(
        interpreter: &mut Interpreter,
        _receiver: Option<Value>,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let total: f64 = args.iter().map(|value| interpreter.number(*value)).sum();
        Ok(Value::from_f64(total))
    }

    /// A native that prints its first argument and hands the text back as a string.
    ///
    /// The one that proves a native can do the two things a native exists to do: look at a value it
    /// was given, and allocate a new one in the heap it was handed.
    fn text_of(
        interpreter: &mut Interpreter,
        _receiver: Option<Value>,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let text = interpreter.display(crate::arg(args, 0));
        interpreter.intern(&text)
    }

    /// A native that throws, because Rust code is allowed to fail the same way an opcode is.
    fn boom(_: &mut Interpreter, _: Option<Value>, _: &[Value]) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Type("boom".to_owned()))
    }

    /// An interpreter with something in its global scope: three natives and one value that is not
    /// callable, which is what a call to it has to complain about.
    fn realm() -> Interpreter {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        interpreter
            .define_global("answer", Value::from_i32(42))
            .expect("should have room");
        interpreter
            .define_native("sum", sum)
            .expect("should have room");
        interpreter
            .define_native("text", text_of)
            .expect("should have room");
        interpreter
            .define_native("boom", boom)
            .expect("should have room");
        interpreter
    }

    /// Run a program in that realm and hand back the value of its last expression.
    fn in_realm(source: &str) -> Result<Value, RuntimeError> {
        realm().run(&as_expression(source))
    }

    #[track_caller]
    fn in_realm_number(source: &str) -> f64 {
        in_realm(source)
            .expect("should not throw")
            .as_f64()
            .expect("should produce a number")
    }

    /// Run a program in that realm and print what its last expression produced.
    #[track_caller]
    fn in_realm_display(source: &str) -> String {
        let blueprint = as_expression(source);
        let mut interpreter = realm();
        let value = interpreter.run(&blueprint).expect("should not throw");
        interpreter.display(value)
    }

    #[test]
    fn a_name_the_embedder_bound_is_a_name_the_program_can_read() {
        assert_eq!(in_realm_number("answer"), 42.0);
    }

    #[test]
    fn a_name_nobody_bound_is_a_reference_error_that_says_which_name() {
        assert_eq!(
            in_realm("missing"),
            Err(RuntimeError::Reference("missing is not defined".to_owned()))
        );
    }

    #[test]
    fn typeof_a_name_nobody_bound_is_undefined_rather_than_an_error() {
        // The one read in the language that is allowed to miss, which is why it is its own opcode
        // rather than a load with a `typeof` after it. If this ever throws, that opcode has been
        // merged with the one above it by somebody who did not know why they were separate.
        assert_eq!(in_realm_display("typeof missing"), "undefined");
        assert_eq!(in_realm_display("typeof answer"), "number");
    }

    #[test]
    fn assigning_to_a_name_nobody_declared_creates_a_global() {
        // Sloppy mode, which is what a script is until lowering can say otherwise. Strict mode
        // throws here instead, and that is the check waiting for a strict flag to check.
        assert_eq!(in_realm_number("undeclared = 7; undeclared"), 7.0);
    }

    #[test]
    fn a_global_written_over_reads_back_as_the_new_value() {
        assert_eq!(in_realm_number("answer = 1; answer"), 1.0);
    }

    #[test]
    fn the_same_name_in_two_places_reaches_one_binding() {
        // What interning buys. The `answer` in the function body and the `answer` at the top level
        // are two mentions in two constant pools, and they have to be the same address or the
        // lookup is a text comparison in disguise.
        assert_eq!(
            in_realm_number("function get() { return answer; } answer = 9; get()"),
            9.0
        );
    }

    #[test]
    fn a_native_is_called_with_the_arguments_the_call_site_passed() {
        assert_eq!(in_realm_number("sum(1, 2, 3)"), 6.0);
    }

    #[test]
    fn a_native_called_with_nothing_is_called_with_nothing() {
        // Not with a slice of `undefined` padding, which is the thing a native that did not check
        // its own length would rather have. The comment in `native.rs` says why the padding is the
        // caller's job and this is the test that holds it to it.
        assert_eq!(in_realm_number("sum()"), 0.0);
    }

    #[test]
    fn a_native_can_read_a_value_and_allocate_a_new_one() {
        assert_eq!(in_realm_display("text(1 + 1)"), "2");
        assert_eq!(in_realm_display("text('katsu')"), "katsu");
    }

    #[test]
    fn a_native_called_from_inside_a_function_reads_that_frame_registers() {
        // The arguments come out of the running frame, so a native called from the top level and a
        // native called from three frames down have to see the same thing.
        assert_eq!(
            in_realm_number("function twice(n) { return sum(n, n); } twice(4)"),
            8.0
        );
    }

    #[test]
    fn a_native_that_throws_stops_the_program_the_way_an_opcode_would() {
        assert_eq!(
            in_realm("boom()"),
            Err(RuntimeError::Type("boom".to_owned()))
        );
    }

    #[test]
    fn a_native_is_a_function_and_prints_like_one() {
        // Which is what Node prints for its own natives, because over there `console.log` is C++ and
        // still prints as `[Function: log]`.
        assert_eq!(in_realm_display("typeof sum"), "function");
        assert_eq!(in_realm_display("sum"), "[Function: sum]");
    }

    #[test]
    fn calling_a_global_that_is_not_a_function_says_so() {
        assert_eq!(
            in_realm("answer()"),
            Err(RuntimeError::Type("42 is not a function".to_owned()))
        );
    }

    #[test]
    fn what_was_bound_can_be_read_back_by_name() {
        // The embedder's half of the same table, which is how a test asserts on a global a program
        // wrote rather than on one it returned.
        let blueprint = as_expression("undeclared = 3; undeclared");
        let mut interpreter = realm();
        interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(interpreter.global("undeclared"), Some(Value::from_i32(3)));
        assert_eq!(interpreter.global("nothing"), None);
    }

    /// A native that writes its first argument to the interpreter's sink and returns nothing.
    ///
    /// A cut down `console.log`, here rather than borrowed from katsu-builtins because this crate
    /// cannot depend on the crate that depends on it, and because what is being tested is the
    /// plumbing and not the builtin.
    #[allow(clippy::unnecessary_wraps)]
    fn print(
        interpreter: &mut Interpreter,
        _receiver: Option<Value>,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let text = interpreter.display(crate::arg(args, 0));
        interpreter.write_output(crate::Stream::Out, &format!("{text}\n"));
        Ok(Value::UNDEFINED)
    }

    /// A realm with a host object in it, which is the arrangement `console` arrives in.
    fn hosted() -> Interpreter {
        let mut interpreter = realm();
        let log = interpreter
            .native_function("log", print)
            .expect("should have room");
        let host = interpreter
            .host_object(&[("log", log), ("version", Value::from_i32(5))])
            .expect("should have room");
        interpreter
            .define_global("host", host)
            .expect("should have room");
        interpreter
    }

    /// Run a program in that realm and hand back the value of its last expression.
    fn hosted_display(source: &str) -> String {
        let blueprint = as_expression(source);
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        interpreter.display(value)
    }

    /// Run a program and print the value of one of its expressions, put through a function that
    /// hands it back.
    ///
    /// A statement that is only a name lowers to no instructions at all, because the value is
    /// already in the variable's own register and a statement discards it. That makes the last
    /// instruction of such a program belong to some earlier expression, so a test that wants to
    /// look at a variable has to make the value be produced by the last instruction. Script
    /// completion values are their own piece of work and are not this one.
    #[track_caller]
    fn evaluate_value(program: &str, expression: &str) -> String {
        evaluate_display(&format!(
            "function id(v) {{ return v; }} {program} id({expression})"
        ))
    }

    #[test]
    fn an_object_literal_builds_an_object_and_prints_the_way_node_prints_one() {
        assert_eq!(
            evaluate_value("var o = { a: 1, b: 'two', c: true, d: null };", "o"),
            "{ a: 1, b: 'two', c: true, d: null }"
        );
        assert_eq!(evaluate_value("var o = {};", "o"), "{}");
        assert_eq!(
            evaluate_value("var o = { 'a-b': 1, _x: 2 };", "o"),
            "{ 'a-b': 1, _x: 2 }"
        );
    }

    #[test]
    fn a_property_of_an_object_literal_reads_back_and_a_missing_one_is_undefined() {
        assert_eq!(evaluate_number("var o = { a: 41 }; o.a + 1"), 42.0);
        assert_eq!(evaluate_display("var o = { a: 1 }; o.b"), "undefined");
    }

    #[test]
    fn a_literal_and_an_object_grown_a_property_at_a_time_agree() {
        // The reason a literal lowers to stores rather than to one instruction taking a list. Both
        // of these walk the same path down the transition tree, so both print the same and both
        // enumerate in the same order.
        assert_eq!(
            evaluate_value("var a = { x: 1, y: 2 };", "a"),
            evaluate_value("var b = {}; b.x = 1; b.y = 2;", "b")
        );
    }

    #[test]
    fn a_duplicate_name_keeps_its_first_position_and_its_last_value() {
        // Two rules at once, and they pull in different directions. The value is the last one
        // written and the position is where the name first appeared, which is what falls out of a
        // store that finds the name already there and does not move it.
        assert_eq!(
            evaluate_value("var o = { a: 1, b: 2, a: 3 };", "o"),
            "{ a: 3, b: 2 }"
        );
    }

    #[test]
    fn an_object_literal_evaluates_its_values_in_source_order() {
        assert_eq!(
            evaluate_value(
                "var n = 0; var o = { first: (n = 1), second: n, third: (n = n + 1) };",
                "o"
            ),
            "{ first: 1, second: 1, third: 2 }"
        );
    }

    #[test]
    fn a_literal_assigned_to_the_variable_its_values_write_is_still_the_literal() {
        // The hazard the lowerer builds into a temporary for. Without that, the object would be
        // built in `x`, the inner assignment would overwrite it with a number, and the store would
        // go nowhere.
        assert_eq!(
            evaluate_value("var x = 0; x = { a: (x = 1), b: 2 };", "x"),
            "{ a: 1, b: 2 }"
        );
    }

    #[test]
    fn an_object_literal_can_hold_another_one() {
        assert_eq!(
            evaluate_value("var o = { p: { q: { r: { s: 1 } } } };", "o"),
            "{ p: { q: { r: [Object] } } }"
        );
        assert_eq!(evaluate_number("var o = { p: { q: 7 } }; o.p.q"), 7.0);
    }

    #[test]
    fn an_object_literal_grows_past_the_room_it_was_built_with() {
        assert_eq!(
            evaluate_number("var o = { a: 1, b: 2 }; o.c = 3; o.d = 4; o.e = 5; o.a + o.e"),
            6.0
        );
    }

    #[test]
    fn a_property_that_is_there_reads_back() {
        assert_eq!(hosted_display("host.version"), "5");
    }

    #[test]
    fn a_property_that_is_not_there_is_undefined_rather_than_an_error() {
        // The rule the whole language rests on. A missing property is not a missing variable, and
        // only one of the two is worth stopping the program for.
        assert_eq!(hosted_display("host.missing"), "undefined");
    }

    #[test]
    fn reading_a_property_of_nothing_says_which_property_it_was() {
        // Node's message word for word, because this is the error a JavaScript programmer reads more
        // often than any other and the words are the useful part of it.
        let blueprint = as_expression("host.missing.deeper");
        assert_eq!(
            hosted().run(&blueprint),
            Err(RuntimeError::Type(
                "Cannot read properties of undefined (reading 'deeper')".to_owned()
            ))
        );
    }

    #[test]
    fn a_method_call_reaches_the_function_the_property_holds() {
        let blueprint = as_expression("host.log(1 + 1)");
        let mut interpreter = hosted();
        let recorder = crate::Recorder::new();
        interpreter.set_output(Box::new(recorder.clone()));
        interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(recorder.text(), "2\n");
    }

    #[test]
    fn a_method_call_on_a_property_that_is_not_a_function_says_which_one() {
        let blueprint = as_expression("host.version()");
        assert_eq!(
            hosted().run(&blueprint),
            Err(RuntimeError::Type("version is not a function".to_owned()))
        );
    }

    #[test]
    fn a_method_written_in_javascript_is_called_the_same_way_a_native_is() {
        // The half of `call_method` that goes back into the loop rather than out of it, which is a
        // different code path from the native call above and is the one that has to get the frame
        // right.
        let blueprint = as_expression("function twice(n) { return n * 2; } obj = twice; obj(21)");
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(value.as_f64(), Some(42.0));
    }

    #[test]
    fn a_property_that_exists_can_be_written_and_a_new_one_can_be_added() {
        let blueprint = as_expression("host.version = 6; host.version");
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(value.as_f64(), Some(6.0));

        // Growing used to be refused, because a record was a fixed set of names decided when it was
        // made. Shapes are what let a store add a name, and the new name goes on the end because
        // insertion order is what the language says enumeration order is.
        let growing = as_expression("host.brand = 'katsu'; host.brand");
        let mut interpreter = hosted();
        let value = interpreter.run(&growing).expect("should not throw");
        assert_eq!(interpreter.display(value), "katsu");
    }

    #[test]
    fn writing_a_property_of_nothing_says_which_property_it_was() {
        // Both halves of the message matter and both are Node's, word for word. The value is named
        // because there is nothing else to name, and the key is named because `a.b.c = 1` failing
        // without saying which link was missing is the error everybody complains about.
        for (source, message) in [
            (
                "var u; u.x = 1",
                "Cannot set properties of undefined (setting 'x')",
            ),
            ("null.x = 1", "Cannot set properties of null (setting 'x')"),
        ] {
            assert_eq!(
                evaluate(source),
                Err(RuntimeError::Type(message.to_owned())),
                "{source}"
            );
        }
    }

    #[test]
    fn writing_a_property_of_a_primitive_is_ignored_in_sloppy_mode() {
        // A number has nowhere to put a property, so sloppy mode drops the write and reading it back
        // gives undefined. That is not a gap, it is what the language says, and it is why the strict
        // mode version of this test exists at all.
        assert_eq!(evaluate_display("var n = 5; n.x = 1; n.x"), "undefined");
        assert_eq!(evaluate_display("var s = 'hi'; s.x = 1; s.x"), "undefined");
    }

    #[test]
    fn writing_a_property_of_a_primitive_throws_in_strict_mode() {
        for (source, message) in [
            (
                "'use strict'; var n = 5; n.x = 1;",
                "Cannot create property 'x' on number '5'",
            ),
            (
                "'use strict'; var s = 'hi'; s.x = 1;",
                "Cannot create property 'x' on string 'hi'",
            ),
            (
                "'use strict'; var b = true; b.x = 1;",
                "Cannot create property 'x' on boolean 'true'",
            ),
        ] {
            let blueprint = crate::compile("t.js", source).expect("should compile");
            assert!(blueprint.strict, "{source} should have been strict");
            let result = Interpreter::new()
                .expect("should reserve a stack")
                .run(&blueprint);
            assert_eq!(
                result,
                Err(RuntimeError::Type(message.to_owned())),
                "{source}"
            );
        }
    }

    #[test]
    fn an_object_that_holds_itself_prints_the_way_node_marks_a_cycle() {
        // Growing is what made this possible: until a store could add a property there was no way to
        // build a cycle, and now there is. The number on the front and the number in the middle are
        // the same number, which is the whole point of the notation.
        let blueprint = as_expression("host.self = host; host");
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(
            interpreter.display(value),
            "<ref *1> { log: [Function: log], version: 5, self: [Circular *1] }"
        );
    }

    #[test]
    fn a_cycle_is_marked_where_the_walk_comes_back_and_not_where_the_depth_runs_out() {
        // The back reference is four levels down, which is past the point where an ordinary object
        // prints `[Object]`. Node still writes `[Circular *1]` there, so the two checks are separate
        // and the cycle check is the one that runs first.
        assert_eq!(
            evaluate_value(
                "var a = {}; var b = {}; var c = {}; a.b = b; b.c = c; c.a = a;",
                "a"
            ),
            "<ref *1> { b: { c: { a: [Circular *1] } } }"
        );
    }

    #[test]
    fn an_object_with_nothing_in_it_prints_as_itself_however_deep_it_is() {
        // The check for empty comes before the check for depth, which was measured against Node and
        // is not what the ordering in the code first said. It matters beyond looking tidier:
        // `[Object]` is six characters longer than `{}` and that is enough to break a line Node
        // keeps whole, so getting it wrong shows up two levels away from where it happened.
        assert_eq!(
            evaluate_value("var o = { p: { q: { r: {} } } };", "o"),
            "{ p: { q: { r: {} } } }"
        );
        assert_eq!(
            evaluate_value("var o = { p: { q: { r: { s: {} } } } };", "o"),
            "{ p: { q: { r: [Object] } } }"
        );
    }

    #[test]
    fn the_same_object_twice_in_different_places_is_not_a_cycle() {
        // The test is whether the walk is inside the object, not whether it has seen it. An object
        // held by two properties of the same parent prints twice with no reference on either.
        assert_eq!(
            evaluate_value("var e = {}; var o = { p: e, q: e };", "o"),
            "{ p: {}, q: {} }"
        );
    }

    #[test]
    fn two_cycles_in_one_printed_value_are_numbered_in_the_order_they_are_found() {
        assert_eq!(
            evaluate_value(
                "var i = { a: 1 }; i.self = i; var j = { b: 2 }; j.self = j; var o = { i: i, j: j };",
                "o"
            ),
            "{\n  i: <ref *1> { a: 1, self: [Circular *1] },\n  j: <ref *2> { b: 2, self: [Circular *2] }\n}"
        );
    }

    #[test]
    fn a_property_added_to_an_object_prints_after_the_ones_it_was_made_with() {
        let blueprint = as_expression("host.brand = 'katsu'; host");
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(
            interpreter.display(value),
            "{ log: [Function: log], version: 5, brand: 'katsu' }"
        );
    }

    #[test]
    fn writing_a_property_that_is_already_there_does_not_move_it() {
        let blueprint = as_expression("host.version = 6; host");
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        assert_eq!(
            interpreter.display(value),
            "{ log: [Function: log], version: 6 }"
        );
    }

    #[test]
    fn the_two_halves_of_an_accessor_in_a_literal_become_one_property() {
        // The setter runs on a write and the getter runs on the read that follows, which is only
        // possible if the second half joined the first instead of replacing it.
        assert_eq!(
            evaluate_number(
                "let o = { v: 0, get x() { return this.v * 2; }, set x(n) { this.v = n; } }; o.x = 21; o.x"
            ),
            42.0
        );
        assert_eq!(
            evaluate_display(
                "function id(v) { return v; } id({ get x() { return 1; }, set x(n) {} })"
            ),
            "{ x: [Getter/Setter] }"
        );
    }

    #[test]
    fn an_accessor_half_written_on_its_own_leaves_the_other_half_missing() {
        assert_eq!(
            evaluate_display("function id(v) { return v; } id({ get x() { return 1; } })"),
            "{ x: [Getter] }"
        );
        assert_eq!(
            evaluate_display("function id(v) { return v; } id({ set x(n) {} })"),
            "{ x: [Setter] }"
        );
        // Reading a property that only has a setter is undefined rather than an error, because
        // there is nothing to call and nothing stored.
        assert_eq!(
            evaluate_display("let o = { set x(n) {} }; o.x"),
            "undefined"
        );
    }

    #[test]
    fn a_value_after_an_accessor_replaces_it_and_keeps_the_place_it_had() {
        // Last mention wins, because a literal defines its properties in source order. The position
        // is the first mention's, which is the same rule an ordinary overwrite follows.
        assert_eq!(
            evaluate_display(
                "function id(v) { return v; } id({ get x() { return 1; }, y: 2, x: 5 })"
            ),
            "{ x: 5, y: 2 }"
        );
        assert_eq!(
            evaluate_display(
                "function id(v) { return v; } id({ x: 5, y: 2, get x() { return 1; } })"
            ),
            "{ x: [Getter], y: 2 }"
        );
    }

    #[test]
    fn an_object_can_grow_past_the_room_it_was_made_with() {
        // Eleven names on an object built with room for two, which is past the inline slots and into
        // the overflow array twice over. The point of doing it through the interpreter as well as in
        // the heap crate's own tests is that the printing has to agree about the order.
        let mut source = String::new();
        for index in 0..11 {
            let _ = write!(source, "host.k{index} = {index}; ");
        }
        source.push_str("host");
        let blueprint = as_expression(&source);
        let mut interpreter = hosted();
        let value = interpreter.run(&blueprint).expect("should not throw");
        let text = interpreter.display(value);
        assert!(text.contains("k0: 0"), "{text}");
        assert!(text.contains("k10: 10"), "{text}");
        assert!(
            text.find("k0: 0") < text.find("k10: 10"),
            "the order names were added in is the order they print in, {text}"
        );
    }

    #[test]
    fn an_object_is_an_object_to_typeof_and_prints_its_contents() {
        assert_eq!(hosted_display("typeof host"), "object");
        assert_eq!(
            hosted_display("host"),
            "{ log: [Function: log], version: 5 }"
        );
    }

    #[test]
    fn an_object_converts_to_text_the_way_the_language_says_and_not_the_way_it_prints() {
        // `console.log` inspects and `+` converts, and the two giving different answers is the
        // language rather than an inconsistency.
        assert_eq!(hosted_display("'' + host"), "[object Object]");
    }

    #[test]
    fn adding_an_object_to_a_number_joins_text_rather_than_giving_not_a_number() {
        // The number on the other side is what makes this worth a test of its own. `'' + host`
        // passes with a `+` that only asks whether either side is already a string, because one
        // side is. This one only passes if the object converts first, and it is the case that was
        // wrong: it read as `NaN` because an object is not a string and nothing ran ToPrimitive.
        assert_eq!(hosted_display("host + 1"), "[object Object]1");
        assert_eq!(hosted_display("1 + host"), "1[object Object]");
        assert_eq!(
            hosted_display("host + host"),
            "[object Object][object Object]"
        );
    }

    #[test]
    fn adding_a_function_to_a_number_joins_text_rather_than_giving_not_a_number() {
        // A closure rather than an object, because it takes the same path: a function is on the
        // heap, so its primitive is a string, so this joins rather than arriving at NaN.
        //
        // The text is not right yet and that is a different piece of work. Node gives
        // `function f() { return 1; }1`, because `ToString` of a function is its own source, and
        // this gives `[Function: f]1` because a closure has a name and a start offset but no end
        // offset and nothing keeps the script text alive to slice. It lands with the source spans
        // that stack traces need. What this test pins is that the operand converted at all.
        assert_eq!(
            evaluate_display("function f() { return 1; } f + 1"),
            "[Function: f]1"
        );
    }

    #[test]
    fn an_object_in_any_arithmetic_other_than_plus_is_not_a_number() {
        // `+` is the only operator that asks whether the primitive is a string. Every other one
        // runs ToNumber on the result, and ToNumber of `[object Object]` is NaN.
        assert_eq!(hosted_display("host - 1"), "NaN");
        assert_eq!(hosted_display("host * 2"), "NaN");
        assert_eq!(hosted_display("-host"), "NaN");
    }

    #[test]
    fn a_string_inside_an_object_is_quoted_and_a_string_on_its_own_is_not() {
        let mut interpreter = realm();
        let text = interpreter.intern("katsu").expect("should have room");
        let host = interpreter
            .host_object(&[("name", text)])
            .expect("should have room");
        assert_eq!(interpreter.display(text), "katsu");
        assert_eq!(interpreter.display(host), "{ name: 'katsu' }");
    }

    #[test]
    fn a_deeply_nested_object_stops_where_node_stops() {
        // Node prints three levels of braces and elides the fourth, which is what `node -e
        // "console.log({a:{b:{c:{d:1}}}})"` writes. Three levels deep still prints in full.
        let mut interpreter = realm();
        let fourth = interpreter
            .host_object(&[("d", Value::from_i32(1))])
            .expect("should have room");
        let third = interpreter
            .host_object(&[("c", fourth)])
            .expect("should have room");
        let second = interpreter
            .host_object(&[("b", third)])
            .expect("should have room");
        let first = interpreter
            .host_object(&[("a", second)])
            .expect("should have room");
        assert_eq!(interpreter.display(first), "{ a: { b: { c: [Object] } } }");
        assert_eq!(interpreter.display(second), "{ b: { c: { d: 1 } } }");
    }

    #[test]
    fn an_object_with_nothing_in_it_prints_as_nothing() {
        let mut interpreter = realm();
        let empty = interpreter.host_object(&[]).expect("should have room");
        assert_eq!(interpreter.display(empty), "{}");
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
