# Tier 0: the bytecode and the interpreter

Most JavaScript in a real process runs a handful of times and never gets hot. Node's own startup, a CLI tool, a serverless invocation, the module initialization of forty dependencies: none of that reaches a JIT tier before the process exits. So the interpreter is not a placeholder we tolerate until the JIT works, it is the thing that determines cold performance, and it is also the reference implementation that document 14 checks the JIT against.

## 5.1 The bytecode

Register based and three address, in the Lua and Ignition lineage rather than a stack machine. A stack machine has smaller code and a slower interpreter, and it makes building SSA in tier 2 harder because you have to reconstruct the value stack. Registers cost a byte or two per instruction and pay for themselves everywhere downstream.

Encoding: one byte opcode, then operands. Registers are one byte, with a `Wide` prefix that promotes every operand in the following instruction to two bytes for functions with more than 256 registers. Constant pool indices are two bytes. Inline cache slot indices are two bytes. Jump offsets are two bytes signed, with a wide form.

No accumulator. Ignition uses one to shrink operands, and it costs an implicit dependency that the IR builder in tier 2 then has to model. We pay the extra byte.

The opcode families:

| Family | Examples |
|---|---|
| Move and load | `Mov`, `LdConst`, `LdUndefined`, `LdNull`, `LdTrue`, `LdFalse`, `LdSmi` |
| Arithmetic | `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Exp`, `Neg`, `Inc`, `Dec`, bitwise, shifts |
| Comparison | `Eq`, `StrictEq`, `Lt`, `Lte`, `Gt`, `Gte`, `In`, `InstanceOf` |
| Property | `GetProp`, `SetProp`, `GetElem`, `SetElem`, `GetPropSuper`, `DefineField`, `DeleteProp` |
| Private names | `GetPrivate`, `SetPrivate`, `BrandCheck` |
| Call | `Call`, `CallMethod`, `CallSpread`, `Construct`, `CallBuiltin` |
| Closures and scopes | `Closure`, `LdUpvalue`, `StUpvalue`, `NewContext`, `LdContext`, `StContext` |
| Control | `Jmp`, `JmpIfTrue`, `JmpIfFalse`, `JmpIfNullish`, `JmpIfUndefined`, `LoopBackEdge` |
| Iteration | `ForInPrepare`, `ForInNext`, `GetIterator`, `IteratorNext`, `IteratorClose` |
| Allocation | `NewObject`, `NewArray`, `NewClosure`, `NewRegExp`, `CloneObjectLiteral` |
| Generators | `Suspend`, `Resume`, `GeneratorStore`, `GeneratorRestore` |
| Exceptions | `Throw` |
| Modules | `LdModuleVar`, `StModuleVar`, `ImportDynamic`, `ImportMeta` |
| Type queries | `TypeOf`, `ToNumber`, `ToString`, `ToObject`, `ToPropertyKey` |

Every opcode that can be polymorphic carries an inline cache slot index as an operand. That includes all property access, all calls, all arithmetic and all comparison, because `+` in JavaScript is a polymorphic dispatch just as much as `o.x` is.

The bytecode is versioned and serializable, because it is simultaneously the interpreter's input, the JIT's input, the AOT compiler's input, and the on disk cache format.

### 5.1.1 The bytecode, as built

`crates/katsu-ir` is four modules: the instruction set, the per function constant pool, the source position table, and the blueprint that holds all three. Everything is re-exported at the crate root, because a consumer wants `katsu_ir::Op` and does not care which file it is written in.

What exists is the decoded form, one Rust enum with one variant per opcode. The byte encoding this document specifies, one byte of opcode then operands with a wide prefix past 256 registers, is not built yet, because nothing writes bytecode to disk until the cache lands. The two will exist side by side rather than one replacing the other: the interpreter matches on the decoded form and the encoding is a lowering of it.

Jumps carry an absolute instruction index rather than a signed offset. An offset is smaller and relocatable and is what the encoded form will use. An absolute index is the one that cannot be wrong by a sign, which matters more while the forward reference patch list in lowering is new.

Every opcode in the set has a construct in the subset that lowers to it. The families with no syntax yet, iteration and generators and modules and private names, are deliberately absent. An opcode with no producer is an opcode whose semantics nobody has had to think about, and it sits in the file looking implemented.

The exception family started as three opcodes in the table above and is one, which is the table design of 4.10 showing up in the instruction set. `PopHandler` was there because a handler stack has to be popped, and there is no handler stack to pop. `ReThrow` was there because a `finally` has to put back whatever it interrupted, and that turned out to be a completion token in a register rather than an opcode of its own. `finally` has now landed and it added no opcode at all, which is the prediction paying off rather than a coincidence: it is entirely a lowering, and the interpreter did not have to change by one line to run it. What is left is `Throw`, which takes a register and never comes back.

Four opcodes exist that the table above does not name. Three are there because a static analysis found something a run time check would otherwise have to find. `LoadUninitialized` writes the hole into a `let` or `const` slot when its scope is entered, and `ThrowIfUninitialized` is the dead zone check, emitted only where scope analysis says a reference can reach one. `ThrowConstAssignment` is the third, and it is an opcode rather than an early error because Node reports assignment to a `const` as a run time TypeError.

`LoadClosure` is the fourth and it is there for a different reason. `const f = function me() { return me; };` binds `me` inside the function and nowhere else, and what it binds is the closure that is running rather than whatever `f` holds by the time the call happens. There is nothing in the frame or the environment to read that from, so it is an opcode, emitted once in the prologue of a named function expression and nowhere else.

`ConstructResult` is the fifth, and it is the second half of `new`. A constructor that returns an object returns that object, and one that returns anything else, a number or nothing at all, hands back the object it was given to build. That rule needs the fresh object to still be somewhere when the call comes back, so `Construct` parks it in the register the callee was read into, which is dead the moment the frame is pushed, and `ConstructResult` picks between that register and the returned value. The alternative was a word on the call frame saying that this frame is a construct, and that word would be read by every return in the program to serve the calls that are constructs. The frame header is thirty two bytes because the last eight cost 14 percent on `call/call_return` when they were there, so the same decision was made the same way: one dispatch per `new`, next to an allocation that costs far more, rather than a load per return.

A blueprint owns the blueprints of the functions written inside it. Handing one to a realm hands over the whole tree, with no second table to keep in step, which is also what makes the on disk cache format one object rather than an archive.

A blueprint can verify itself, and this is worth more than it sounds. It checks that every jump lands inside the code, every register fits the frame the function sized, every constant and cache slot and nested function index exists, and that the code ends in a terminator. Those are exactly the mistakes a lowering pass makes, and every one of them produces bytecode that runs and is wrong rather than bytecode that fails to load. The check runs in every lowering test rather than only in debug builds.

There is a disassembler, because a test that asserts on a listing says what it means and a test that asserts on a vector of enum variants is unreadable at the moment it fails.

The constant pool deduplicates, and it holds Rust strings rather than atoms. Spec 4.5 says strings are interned isolate wide and they will be, but not here: `katsu-parse` and `katsu-gc` are both at layer 2 and neither can depend on the other, so the pass that fills the pool cannot reach the atom table. The pool is the list of things to intern and the index into it is the operand, so interning costs one walk when a realm loads the blueprint and nothing per execution. Spec 4.1.1 predicted this would happen at lowering because lowering would sit above both crates, and that prediction was wrong: lowering lives next to the tree it reads.

Numbers are keyed in the pool by bit pattern, so `0.0` and `-0.0` stay separate entries, which they have to, because `1 / -0` is not `1 / 0`.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Add a string the pool has not seen | 37.2 ns | 48.8 ns | 66.4 ns |
| Add a string the pool already holds | 5.0 ns | 4.9 ns | 5.2 ns |
| Add a number | 16.6 ns | 13.7 ns | 16.3 ns |

Measured over batches of 512 at one commit on the three reference machines from document 15.5. The second row is the one real code lives on, because a program that reads `.length` reads it in twenty places, and a repeat costs a hash and no allocation. The first row is a fresh pool growing from empty, which is what the first function lowered in a file pays. Windows being half again slower than Linux on the same silicon in the first row and identical in the second says the gap is the allocator and not the pool.

The benchmark paid for itself immediately. Keying numbers on `f64::to_bits` directly made adding 512 numbers cost more than adding 512 strings, even though the string path also allocates, which is backwards. The reason is that the numbers a program writes down are loop bounds and array indices and small decimals, and every one of those is a double whose mantissa is nearly all zeroes, so the low bits that choose the hash bucket were nearly identical across the whole pool and every insert walked a long probe chain. Running the key through the splitmix64 finalizer first took a number from 45 nanoseconds to 15. The finalizer is a bijection, so it cannot merge two different doubles into one entry, and there is a test that says so.

The source position table stores a byte offset rather than a line and a column. Turning an offset into a line costs one scan of the source and happens when a human is about to read it, which is rare, while storing two numbers per entry would cost memory in every function that is ever loaded, which is not. One entry per run of instructions that share a position, found by binary search, so a table for a large function is far smaller than its instruction count. A varint delta encoding would be smaller still and is the obvious thing to do when the memory census in spec 08.7 says this table is worth shrinking.

The format version is 2. Version 1 was the eight opcode sketch that shipped in 0.0.1, nothing ever wrote it to disk, and bumping it costs nothing while pretending the format did not change costs somebody a confusing afternoon.

## 5.2 One semantic description, three consumers

Each opcode's semantics are written once, in a macro DSL, and three things are generated from it: the interpreter handler, the tier 1 stencil, and the tier 2 IR builder rule.

This is the Deegen result applied to our situation. Deegen generates an interpreter, a baseline JIT and the tier switching logic from bytecode semantics written as C++ functions, and the generated Lua interpreter came out 1.31x faster than LuaJIT's hand written assembly one while the generated baseline JIT landed within 33% of LuaJIT's optimizing JIT. Druid does the same trick for Pharo from an annotated interpreter, generating 13.4k lines of compiler frontend equivalent to a hand written 3.3k while touching only 60 call sites in the interpreter.

The reason to do it is not code volume, it is drift. The classic multi tier VM bug is that the interpreter and the baseline JIT disagree about one edge case, so the program is correct for a thousand iterations and then silently wrong. Generating both from one description makes that class of bug unrepresentable, and document 14's differential fuzzer then covers the cases the generator itself gets wrong.

A sketch of what a handler looks like in the DSL:

```
opcode! {
    Add(dst: Reg, lhs: Reg, rhs: Reg, ic: IcSlot) {
        // fast paths become both the quickened interpreter variants
        // and the specialized stencils
        fast if (lhs: Int32, rhs: Int32) => {
            let (v, overflow) = lhs.overflowing_add(rhs);
            if overflow { fallthrough }
            dst = Value::int32(v)
        }
        fast if (lhs: Double, rhs: Double) => { dst = Value::double(lhs + rhs) }
        fast if (lhs: String, rhs: String) => { dst = concat(lhs, rhs) }
        slow => { dst = generic_add(vm, lhs, rhs, ic) }
    }
}
```

The generator emits: an interpreter handler with the fast paths inline and the slow path outlined; a set of stencils, one per fast path plus one generic; a quickening rule that rewrites the opcode in place to the variant the feedback observed; and a tier 2 IR rule that emits a typed add with a guard when the feedback says the site only ever saw int32.

Building this generator is a real chunk of work and it lands in M2 in document 13, before the tiers depend on it.

## 5.3 Dispatch, on stable Rust, in 2026

The classic fast interpreter dispatches with a tail call from the end of each handler to the next, so that each opcode gets its own indirect branch site and the branch predictor can learn opcode pair correlations. In C you write this with `musttail`. In Rust you write it with `become`, and `become` is nightly only behind `feature(explicit_tail_calls)`, documented as incomplete, with LLVM codegen landed but other backends stubbed. The Trifecta Tech project goal plans the work through 2026 and targets stabilization in **2027**, contingent on funding.

We cannot build the product on a nightly feature that might stabilize in eighteen months. So the dispatch strategy has three implementations behind one flag, and document 15 measures them:

**A. `loop { match op }` on stable.** One indirect branch for the whole interpreter. Portable, safe, works today, and modern branch predictors handle it better than the folklore suggests. This is the default and the thing everything else is measured against.

**B. `become` on nightly.** Tail called handler functions, one dispatch site per opcode. Turned on by a feature flag for anyone building with nightly, and it becomes the default the day it stabilizes.

**C. Stencil threaded.** Because we already have to build stencils for tier 1, we can also stitch a threaded interpreter loop out of the same stencils at startup, giving us a computed goto interpreter without needing any Rust language feature. This is what Deegen effectively does. It costs a runtime code buffer, which the memory budget notices, and it requires the build time toolchain step anyway.

There is also `#[loop_match]`, the experimental computed goto style work in rustc, which is worth tracking as the shortest path to A becoming as fast as B without nightly tail calls.

**The decision rule:** ship A, keep B behind a flag, prototype C during M2 alongside the stencil work, and pick by measurement at M3. Do not let the interpreter's dispatch strategy become a blocker for anything else, because tier 1 exists precisely so that the interpreter's ceiling does not determine the product's ceiling.

### 5.3.1 The dispatch loop, as built

`crates/katsu-vm/src/interpret.rs` is strategy A, written flat. Every opcode is one arm of one match, the arm does the work, and nothing is hidden behind a generic helper that takes a closure. That costs some repetition and it buys the property 5.2 is actually asking for, which is that the semantics of an opcode are readable in one place. It also means an arm can be split into a fast path and a slow path when the quickening in 5.5 arrives, without unpicking an abstraction first.

Everything that happens inside a single frame runs: the loads and moves, arithmetic, the bitwise operators, comparisons, the unary operators, the temporal dead zone checks, jumps, back edges and `return`. Strings run too, now that the interpreter owns an isolate: a string literal reaches a register, two of them concatenate, they compare by code unit and they convert to numbers and booleans by the rules the standard gives.

Calls, closures and environments run as well. A call pushes a frame, copies the arguments out of a run of the caller's own registers, runs the callee and writes what it returned into the register the call names. A function nested inside another one becomes a closure over the environment it was written in, and a variable a closure captures moves out of a register and into a context, which is a heap object holding one cell per captured variable and a pointer to the context outside it. Recursion works, a closure outlives the call that made it, two closures over the same function get a cell each, and a program that never stops calling itself gets the depth limit rather than a segmentation fault.

Globals run, and so does the second kind of call. A name the program did not declare is looked up in a map on the isolate, a name nobody bound is a `ReferenceError` that says which name, and `typeof` on a name nobody bound is `undefined` rather than an error, which is the one place in the language where reading an unbound name is legal. Assigning to a name nobody declared creates it, because that is what a sloppy mode program does. The map is a map rather than an object with a shape, and 07.4.2 says why that is honest rather than temporary: `delete`, property descriptors, a prototype and enumeration are all still ahead, and none of them exist to be got wrong yet. A call whose target is a function written in Rust rather than in JavaScript now runs too, which is what gives an embedder a way to put something in a realm that the program can call.

Reading a named property, writing one and calling a method all run now, against an ordinary object with a shape. 07.4.2 says what that object is and what it deliberately cannot do yet. A name the object does not have reads as `undefined`, because that is the rule the whole language rests on, and reading a property of `undefined` or `null` throws the message Node throws word for word. A write can now add a name, which is what shapes bought and is what a record could never do, and the name goes on the end because insertion order is what the language says enumeration order is.

Writing has three outcomes rather than one, and all three are the language rather than gaps. A write to an object stores. A write to `undefined` or `null` throws, because there is nothing there to write to. A write to a number, a string or a boolean has nowhere to go, so sloppy mode drops it and strict mode throws `Cannot create property 'x' on number '5'`, naming the value as well as its type. Both messages are Node's word for word, and the strict case is reachable because the blueprint already knows whether the function it came from is strict.

An object literal runs, and it runs as a `new_object` followed by one `set_prop` per property rather than as one instruction that takes a list. That is three or four instructions where one would do and it is deliberate, because a store is the operation that takes a shape transition, so building a literal out of stores means a literal and an object grown a property at a time reach the same shape and neither of them needs a second code path. It also means every property of a literal is already an inline cache site without inventing a new kind of cache. The count in `new_object` is a promise about what the stores after it are about to put in, so a literal is one allocation with room rather than an allocation and then a properties array, and an object that ends up with more than it was built for grows the ordinary way. The one thing lowering has to be careful about is that a literal writes its destination before its operands run, since the stores need somewhere to store into, so `x = {a: (x = 1)}` builds into a temporary rather than into `x`.

Exceptions run, which is the first control flow in the interpreter that is not a jump inside one function. `throw` is one opcode that never returns to the instruction after it, entering a `try` is no opcode at all, and `Interpreter::handle` is what decides where a throw goes by reading the table 4.10 describes. The search crosses frames rather than stopping at the one that threw, so a throw at the bottom of a call chain finds a handler above it and every frame in between is popped without finishing, which is the whole reason `throw` is worth having. What a `catch` binds is a register like a function parameter, written by the search itself when it finds the handler, and a captured one lands in a temporary that the first instruction of the handler copies into its cell.

Three things are deliberately not catchable, and they are a list in one function rather than a flag on each error: running out of memory, being asked to stop, and reaching an opcode this build has not written yet. The first two are not catchable in Node either, and the third is a gap in katsu rather than an event in the program, so letting a `catch` swallow one would turn a missing feature into a wrong answer. Everything else is, including a stack overflow, which Node reports as a catchable `RangeError` and so does this.

An engine error stays a name and a message until something catches it, and becomes an object at the moment a handler takes it. Almost no `TypeError` is ever caught, so the allocation is paid at the `catch` rather than at the throw, and 5.3.7 measures what that is worth and what is still left to move. What it becomes is now a real instance of the right error prototype, built from a table the isolate holds rather than from the global of the same name, so `e instanceof TypeError` is true and rewriting `TypeError` in the program changes what the program's own `new TypeError` means and leaves what the engine throws alone. A realm with no standard library installed still gets the two property object, because there is nothing there for an error to be an instance of, and the interpreter's own tests run in one.

What is still missing from an engine error is `stack`, which needs frames that outlive the throw and source spans to name them. That is the one difference `console.log(e)` still shows: Node prints a trace and katsu prints the bracketed form Node itself uses for an error with no stack on it. Every other part of the printed form is Node's, including the rule that hides a `name` the first line already says and the rule that shows a `cause` the constructor made hidden.

`finally` runs, and it runs without an opcode of its own. What makes that possible is that the five ways out of a guarded block become a number in a register, so the thing the interpreter has to do is compare an integer and take a jump, which it already knows how to do. The lowering is 4.5.1 and the cost is 5.3.7. The one visible consequence for this document is that the handler table now has entries whose target is a `load_int` rather than the first instruction of a user written `catch`, and nothing in the search cares, because the search only ever asked which range contains the throwing instruction.

A key computed at run time and `delete` still do not run, so those fall through to the arm at the bottom of the match and produce an error naming the opcode. A refusal that says `get_index is not implemented yet` is worth a great deal more than a wrong answer, and it is the difference between a runtime that is incomplete and one that is untrustworthy. The parser refuses the parts of object literal syntax that have nothing to build against yet, each by its own name, so a spread, a getter, a setter, a method, a computed name and a numeric name each say what they are rather than sharing one message.

`ToPrimitive` is where an object meets the operators, and it is one function with three callers rather than a special case in each of them. `+` converts both sides and then asks whether either is a string, which is not the same as asking whether either is a string and converting after: `{} + 1` is the text `[object Object]1` and neither operand was a string on the way in. The relational operators convert before the string test for the same reason, which is why `'9' < {}` is true and is a code unit comparison rather than a comparison of two NaNs. Loose equality treats two objects as the strict question, because two objects are equal when they are the same object and never because their contents match, and an object against a primitive converts and asks again. A conversion now calls the `toString` an object inherits, as long as that one is written in Rust, which is what makes `'' + err` say `TypeError: message` rather than `[object Object]`. One written in JavaScript is still not called, because calling a function from the middle of a conversion means running a frame to completion from inside an opcode and the loop cannot do that yet. The hint the standard passes still makes no difference and is still not threaded through, because the only `valueOf` in the realm is the one on `Object.prototype` that hands back the object it was called on, and `ToPrimitive` moves on to `toString` when it does. The first builtin whose `valueOf` answers with a primitive is the one that has to add the other half. Since a conversion can now call something, it can also nest, so it counts its own depth and raises the same `RangeError` runaway recursion raises rather than growing the real stack until the process dies.

`console.log` prints an object by inspecting it rather than converting it, and what that looks like is not specified anywhere, so `crates/katsu-vm/src/inspect.rs` holds the rules and every one of them was read off Node by running it. Which property names are printed bare, which quote a string gets, how a control character is escaped, how deep the walk goes before it writes `[Object]` and the arithmetic that decides where a line breaks are all Node's, and the differential harness compares byte for byte, so a rule that is nearly right is a failing test rather than a cosmetic difference.

An object that can be reached from inside itself prints the way Node marks one, with `<ref *1>` in front of the object and `[Circular *1]` where the walk found its way back to it. Three details there are each a place to be wrong and each was measured. The test is whether the walk is currently inside the object and not whether it has seen it, so the same object held by two properties of one parent prints twice with no reference on either. The number is handed out where the way back is found rather than where the object was first printed, so the first cycle in reading order is always the one numbered one, and the numbering starts again for each value `console.log` is given. The cycle check runs before the depth check, so a cycle four levels down says `[Circular *1]` rather than `[Object]`. And the reference counts towards the width arithmetic even though it sits outside the braces, so an object carrying one breaks eight characters earlier than the same object without one.

The numeric conversions live in their own module, because each of them is a place where the obvious Rust expression is subtly not what JavaScript says. `ToInt32` is a modulo and a fold rather than a cast, so `1e10 | 0` is `1410065408` while `1e10 as i32` saturates. A shift count is taken modulo thirty two, so `1 << 32` is `1`. Exponentiation disagrees with IEEE `pow` in exactly two places, `1 ** NaN` and `(-1) ** Infinity`, both of which are `NaN` in JavaScript and one in IEEE. Relational comparison between two numbers is the one case where the obvious expression is right, because Rust's float operators are the IEEE ones and already return false on both sides of a `NaN`, which is what the standard asks for. That is why each of the four relational opcodes uses its own operator on its fast path rather than sharing one helper that returns a three way answer. The standard's abstract relational comparison really does have three outcomes, less, not less, and undefined for a `NaN`, but the undefined case only becomes visible once the code has to decide which way to negate, and it only has to decide that on the slow path. On the fast path `a >= b` is a single instruction that is already correct.

Back edges check one shared atomic word, which is the mechanism 5.6 describes, and a test proves an endless loop can be stopped from another thread. That check is the only per iteration cost the loop pays that a straight line of instructions does not.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Move a register | 0.79 ns | 0.87 ns | 0.85 ns |
| Add two numbers | 1.55 ns | 2.15 ns | 2.22 ns |
| Compare two numbers | 1.53 ns | 1.42 ns | 1.51 ns |
| Raise to a power | 3.29 ns | 8.20 ns | 4.14 ns |
| One counting loop iteration | 6.94 ns | 6.00 ns | 6.18 ns |
| The same, per instruction | 1.74 ns | 1.50 ns | 1.55 ns |

Measured on the three reference machines from document 15.5, a thousand instructions to a run so that the frame push either side is noise. The two gamingpc columns are the same silicon pinned to one performance core, once under WSL2 and once natively, and they are the columns that decided anything here. The m4 column is a laptop with other work on it and no way to pin a core, and across reruns it moves by more than the differences discussed below, so treat it as a sanity check on the order of magnitude and not as a measurement.

A move is the floor, because it reads one register and writes another and everything left over is the fetch, the bounds check and the branch the match compiles into. Under a nanosecond on all three machines is a good deal better than the folklore about a single dispatch site predicts, and it is the number strategies B and C have to beat.

The interesting result is that an add costs more than a comparison, on every machine, when the two arms do identical work apart from the operator. The difference is entirely in what happens to the result. A comparison writes a boolean, which is a tag and nothing else. An add writes a number through `Value::from_f64`, which checks whether the double is exactly an integer and re-encodes it as a tagged integer when it is, so that the next instruction to read it gets an integer rather than a boxed double. That check is the right default and it is also the single clearest argument for the integer quickening in 5.5: an `Add.Int32` that stays in integers throughout never converts to a double and never converts back, and the gap between the add row and the compare row is roughly what it stands to win.

Exponentiation is the one operator that calls into libm rather than compiling to an instruction, and it is the only row where the same silicon disagrees with itself. The 13900K runs it in 8.20 ns under WSL2 and 4.14 ns natively, which is glibc's `pow` against the Microsoft runtime's and not anything we wrote. It is worth knowing before anybody reads a benchmark that leans on `Math.pow` and concludes something about the engine.

The counting loop is the row that resembles a real program: a comparison, a conditional jump, an add and a back edge, with a dependency between every pair of them and a branch for the predictor to get right. Six nanoseconds an iteration, about 1.5 ns an instruction, which is close enough to the straight line rows to say the branch is predicted and the interrupt check is close to free. No comparison against Node is claimed here, because a fair one needs the differential harness and a program that both runtimes can actually run.

### 5.3.2 Three things that moved these numbers and are not in the arm

Teaching the interpreter about strings should not have touched arithmetic at all, and the first attempt made `add_chain` and `compare_chain` 167 percent slower. Every one of the three causes turned out to be something other than the code in the arm being measured, which is worth writing down, because the same three will come back every time an opcode grows a second case.

**A conversion inlined into the switch.** The first version of `Add` asked whether either side was a string before it asked whether both sides were numbers. That is the readable order and it is the wrong one, because asking whether a value is a string means following a pointer into the cage, which puts a bounds check in front of every addition in every program that never touches a string. Worse, the number conversion grew large enough that inlining it dragged a string decoder into the middle of the dispatch switch, where it competed for registers with the loop. The fix is the shape the rest of the interpreter now uses everywhere: an `#[inline]` wrapper that tests `Value::as_f64` and nothing else, and a `#[cold] #[inline(never)]` function holding every other case. Addition on the pinned Linux machine went from 2.77 ns to 2.15 ns, which is 22 percent faster than before strings existed at all.

**Cold is not the same as out of line.** `ToBoolean` is what every conditional jump goes through, and its heap case was marked `#[inline(never)]` but not `#[cold]`. That produced a reproducible 32 percent regression in the counting loop on Windows while Linux on the same silicon was flat, which took three rounds of measurement to believe. A call on a hot path forces the loop's live values into callee saved registers whether or not the call is ever taken. Marking it cold moves the spilling into the branch that is not taken. Adding one attribute took the Windows counting loop from 6.63 ns an iteration to 6.18 and made every row on that machine faster than it was before strings.

**The size of the struct the loop holds.** An isolate is two hundred and eighty bytes of heap bookkeeping that the dispatch loop only reads when a string is involved. Holding it inline made `Interpreter` three hundred and forty four bytes instead of seventy two, and that alone cost a move 0.86 ns against 1.07 ns. Reordering the fields with `#[repr(C)]` and putting the stack first recovered almost none of it, so it is the size and not the offsets. Putting the isolate behind a `Box` recovered nearly all of it. This is the least intuitive of the three and the easiest to reintroduce, since the natural instinct when adding state to the interpreter is to add a field.

Each of these was isolated on a throwaway branch measured on a pinned core rather than argued about from the assembly, which is the only method that worked. Two of the three would have been invisible on the laptop.

### 5.3.3 What a string costs

`Add` and `Less` are each two opcodes now, and the question a reader has is not what a concatenation costs on its own but how much the string case costs over the number case that was already there.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Concatenate two short strings | 51.3 ns | 60.2 ns | 118.9 ns |
| Compare two short strings | 5.97 ns | 5.36 ns | 5.45 ns |

Measured with two Latin-1 literals that differ in their first code unit, so the comparison decides on the first byte it reads and what is left is the call and not the length of the string.

Comparing two strings is around four times what comparing two numbers costs, and that is about the floor for it. Both sides have to be decoded from the cage, both headers have to be read to learn whether they are Latin-1 or UTF-16, and the mixed case has to widen one side a code unit at a time. Four times a number comparison for all of that is a reasonable place to start, and none of it is on the path of a program that only compares numbers.

Concatenating is a different order of magnitude, because it allocates. Sixty nanoseconds is roughly thirty additions, and essentially all of it is the allocation and the copy rather than the dispatch. The Windows number is nearly double the Linux number on the same silicon, which is the one figure here that is not yet explained. The likely cause is the first touch of freshly committed pages, since this benchmark deliberately runs on a fresh isolate every iteration, and Windows charges more for that fault than Linux does. That is a guess and it is written down as one. It is also the strongest argument in this document for rope strings, which document 07 already plans: the cost being measured is the copy, and a rope does not do the copy.

The benchmark uses a fresh isolate per iteration because every concatenation allocates and there is no collector to take it back yet. That is the only benchmark in the workspace whose shape is dictated by a milestone that has not landed, and it goes back to an ordinary persistent interpreter once M4 gives the heap a way to reclaim. Until then the concatenation number carries a fixed setup cost that a real program would pay once, so read it as an upper bound.

### 5.3.4 What a call costs

Three benchmarks, all compiled from source rather than assembled by hand, because the point is what a program pays and not what an opcode costs in isolation. A thousand `id(1)` statements against a function whose whole body is `return x`, which is the shortest call there is. A thousand calls through a closure whose body reads one variable it captured, which is what any function that uses a name from where it was written pays on top. And `fib(20)`, which is recursion with a real stack under it and the oldest benchmark in the business.

`fib(20)` makes 21,891 calls, counting the outermost one. The count satisfies `C(n) = 1 + C(n-1) + C(n-2)`, which closes to `2 * F(n+1) - 1`, and `F(21)` is 10,946. That number is in the benchmark as a constant so that criterion divides by it and reports the cost of one call rather than the cost of one tree.

| Operation, per call | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| A call and a return | 7.01 ns | 9.46 ns | 9.90 ns |
| The same through a closure that reads one captured variable | 14.71 ns | 15.49 ns | 15.87 ns |
| A call inside `fib(20)` | 20.42 ns | 23.24 ns | 23.50 ns |

The Windows column is the lower of two runs of every row, because that box moves by up to a fifth between runs in one direction only, and background work makes a benchmark slower rather than faster. The gamingpc column was taken with a load average of about 3.6 on an otherwise idle desktop, which is WSL2 doing its own housekeeping.

A bare call and return is seven to ten nanoseconds, which is close to the stack work in 5.4.1 plus the dispatch for the two opcodes around it, so there is nothing hiding in the call path that the frame does not explain. A call inside `fib` costs three times that, and it should: each call runs a comparison, a conditional jump, two subtractions, an addition, two `return` paths and the two calls themselves, so about ten instructions at the one and a half nanoseconds 5.3.1 measured, on top of its own frame. Twenty nanoseconds is roughly what those parts add up to.

The row that does not add up is the closure. Reading one captured variable and adding it doubles the cost of the call, and a load and an add are not seven nanoseconds of work. The read goes through the frame header to find the context, decodes a slot, checks that it really points at a context, bounds checks the cell index against the context's length and builds an `Option` at each step, which is four dependent loads and four branches to fetch one value that a real engine gets in one instruction. It is correct and it is measured, and it is the first thing the quickening in 5.5 should take, because a captured read has exactly the shape an inline cache wants: the hop count and the slot are known at compile time and the only thing that varies is the context.

Node is the point of comparison and there are two of them, because comparing our interpreter against a fully warmed optimizing compiler answers a different question than comparing it against Node's interpreter.

| `fib(20)`, per call | m4 | gamingpc-win |
|---|---|---|
| katsu, interpreter only | 20.42 ns | 23.50 ns |
| Node 26 with `--jitless`, so V8's interpreter only | 14.13 ns | 17.78 ns |
| Node 26 as it ships | 1.62 ns | 1.60 ns |

Measured with the same `fib(20)`, two hundred calls of warmup and then a thousand timed runs through `performance.now`, on the same machines and with the same pinning as the rows above. There is no gamingpc column because the WSL2 side of that box has no Node installed, and `--jitless` turns off the optimizing tiers and the baseline compiler and leaves Ignition, which is the closest thing V8 has to what we have today.

Two honest readings come out of that table. Our tier 0 is within about a third to a half of V8's tier 0 on this shape, which is a reasonable place to be for an interpreter that is a few weeks old against one that has been tuned for a decade, and the gap is roughly the closure row above plus the integer quickening that 5.5 has not built yet. And Node as it ships is twelve to fifteen times faster than either interpreter, because TurboFan has compiled `fib` into machine code with the recursion inlined and the small integer arithmetic unboxed. That is the number the goal in 02 is about, and nothing in this document closes it. Tier 1 in document 06 is what has to.

### 5.3.5 What a global and a Rust function cost

Two more benchmarks compiled from source, on the same shape as the rest: a thousand statements that read one global, and a thousand statements that write one. A third calls a function written in Rust a thousand times through a local binding, so that it is the same call site as the `id(1)` row in 5.3.4 with a different thing on the other end of it.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Move a register, for scale | 1.19 ns | 1.10 ns | 1.47 ns |
| Read a global | 2.61 ns | 3.10 ns | 3.44 ns |
| A statement that writes a global | 5.58 ns | 4.81 ns | 5.61 ns |
| Call a function written in Rust | 6.51 ns | 6.29 ns | 9.09 ns |
| A call and a return, in the same run | 7.83 ns | 9.45 ns | 10.01 ns |

The write row is a whole statement rather than one opcode, because `answer = 1` lowers to a load of the constant and then the store, and separating them would mean measuring an opcode that no program emits on its own. The last row is the `id(1)` benchmark from 5.3.4 re-measured in the same run as the rows above it, which is what makes it comparable with the row over it. It agrees with the published figures there to within the drift that box shows from one day to the next, and the m4 column is the lower of two runs because two runs of identical code on that laptop on the same afternoon disagreed by forty percent, which is the clearest demonstration yet of why the reference machines exist.

Reading a global costs about two nanoseconds over a register move, and that two nanoseconds is a hash and a probe. It is a hash of four bytes rather than of the name, because every name a program mentions is interned and the key is the address the interning produced, so `answer` is hashed once when the program is compiled and never again while it runs. That is the cheap version of this and it is still two nanoseconds on every global read, which is what the quickening in 5.5 is for. The catch is that a load site cannot cache a pointer to the entry until the entry stops moving, and entries in a hash map move when it grows. Making them stable is the same M1 work that turns this map into a real object with a shape, so the quickening waits for it rather than racing it.

The result worth keeping is the last two rows. A function written in Rust is a third cheaper to call than a function written in JavaScript, on the same call site, on the pinned Linux machine. It should be: it pushes no frame, it never re-enters the dispatch loop, and the `return x` the JavaScript version runs is an opcode that does not exist on this path. What it does pay is a kind tag read through the cage, an ordinal, a bounds checked index into the isolate's table, and a copy of the arguments into a small vector on the Rust stack. The copy is not laziness. The native takes the interpreter, and its arguments live in that same interpreter's register stack, so a slice borrowed straight out of it would be a second borrow of the thing being passed. Up to eight arguments the copy stays inline and never allocates. The Windows column shows a much smaller margin for reasons that box has not explained yet, and it moves enough between runs that it is not worth a theory.

The three new opcode arms and the native path in the call arm cost the rest of the loop nothing measurable. Against the previous release on the same machine on the same day, a move is two percent faster, a call and return four percent faster, a closure call one percent slower and `fib(20)` one percent faster, which is scatter in both directions rather than a change. That check is worth running every time the match grows, because 5.3.2 records two occasions where it would have caught a real regression that nothing else would have.

### 5.3.6 What a property costs, and one bug the measurement found

The operation 04 says the whole architecture is judged on, measured in the form it has before there is a shape to cache against. The object has eight names on it, which is about what a real host object carries, and every benchmark reaches for the last of them, because the lookup is a linear scan and the last name is the worst case and therefore the honest one to publish.

All three reference machines were under other work while these ran, and every absolute number below is between ten and fifty percent above what the same machine produced for the same code earlier in the same session. So the table reports each machine's own scale alongside its property numbers, and the durable statement is the ratio within a column rather than any single figure. The absolute table in 5.3.5 stands as measured on a quiet box and is not restated here from a noisy one.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Move a register, for scale | 1.35 ns | 1.66 ns | 1.80 ns |
| Read a global, for scale | 2.61 ns | 3.33 ns | 4.14 ns |
| Read a property | 7.63 ns | 12.38 ns | 12.48 ns |
| A statement that writes a property | 7.58 ns | 11.94 ns | 13.94 ns |
| Call a method | 13.08 ns | 18.63 ns | 19.14 ns |

A property read is about three times a global read and about six times a register move, and the three machines agree on those two ratios to within the spread the load explains. Eight compares of four byte addresses inside one cache line comes out at roughly a nanosecond and a half per compare, which is what an unrolled loop with a bounds checked read at each step costs and is not surprising. A method call is a property read plus a call, and it adds up: on the m4 the read is 7.63, a Rust call measured in the same run is 7.02, and the method call is 13.08.

That is the number M1 has to beat, and writing it down is the point of measuring it. An inline cache does not make the scan faster, it removes the scan, so the comparison M1 owes is a guard against a shape versus this, on the same benchmark, on the same machine.

The measurement paid for itself immediately. The first run of it reported 43 nanoseconds for a property read on Windows, more than three times what the scan could possibly cost, and the cause was not in the lookup at all. The opcode arm was building the text of the property name for the error message it might need, on the way through, whether or not it was going to fail, and reading a constant back out of the pool allocates a `String`. Moving the message into a cold function that a successful read never calls took the same benchmark from 43.0 to 11.8 nanoseconds on the same machine minutes apart. It is the same lesson as all three in 5.3.2: the cost was in code that was there for the case that does not happen, and nothing but a benchmark was ever going to point at it.

### 5.3.7 What a `try` costs when nothing goes wrong, and what a throw costs when something does

The handler table of 4.10 is a trade, and both sides of it are measurable, so both sides are measured here rather than argued about. Every number is a thousand iterations of a `while` loop divided by a thousand, so they are per iteration and not per instruction, and the loop is the same loop throughout.

These ran on `gamingpc-win`, pinned to one performance core with `start /affinity`, which is why the intervals are tight enough that a four nanosecond difference is a result rather than a mood. The earlier version of this table was measured on the m4 and has been replaced rather than extended, because the m4 was indexing photos throughout the session these were taken in and moved by more between reruns than several of the differences below. `gamingpc` under WSL2 was not reachable. One column that can be trusted is worth more than three that cannot, and the table is for the ratios inside it anyway.

| Operation | gamingpc-win |
|---|---|
| An iteration of a plain loop, for scale | 14.29 ns |
| The same iteration wrapped in a `try` that never fires | 15.14 ns |
| The same iteration wrapped in a `finally` that always runs | 19.09 ns |
| An iteration that throws a number and catches it in the same frame | 16.36 ns |
| The same throw travelling through one `finally` on its way out | 27.76 ns |
| A call and a `return`, for scale | 23.98 ns |
| The same `return` routed through a `finally` | 41.57 ns |
| A throw caught three frames up | 51.54 ns |
| A caught `TypeError` instead of a caught number | 196.58 ns |

The first two rows are the claim the design exists to make, and the honest version of it is not zero. A `try` that never fires costs 0.85 ns, which is one `jump` per exit, over the handler and into the code after it, the same jump an `if` with no `else` emits, plus one register of frame width for the slot the caught value would land in. Nothing is pushed on entry and nothing is popped on exit, because there is no handler stack to push onto, and that is the part the design was for. Both remaining costs come from lowering the handler in line, and lowering it out of line would remove them, which is a thing to do rather than a thing to explain.

The third row is what a `finally` costs when nothing goes wrong, and it is 4.80 ns over a plain loop and 3.95 ns over a `try` and `catch`. That is three instructions on the normal path, the `load_int` that sets the token to zero, the jump into the body and the `jump_if_false` that lets a normal completion out of the dispatch, plus two registers of frame width. Against 1.55 ns per instruction from 5.3.5 that is the whole of it, so a `finally` costs what its instructions cost and nothing is hiding. It is four to five times what a `catch` costs, and the reason is not the token, it is that a `catch` gets to be zero instructions on the path that does not throw and a `finally` cannot be, because it runs on that path too.

The fourth row is the one that looks wrong and is not. An iteration that throws costs about what an iteration that does the arithmetic costs, because throwing skips the arithmetic and the jump and replaces them with one `throw`, a walk of a one entry table and a store into the handler's register. The useful reading is not that throwing is free but that a throw caught nearby is in the same range as ordinary instructions rather than an order of magnitude above them, which is what a table search should cost when the table is short.

The fifth row prices a `finally` in the path of a throw at 11.40 ns, which is the prologue that sets the token, the dispatch, and a second walk of the table for the `throw` the dispatch re-issues. The second walk is the honest cost of not chaining: a completion that passes through two `finally` clauses searches the table twice rather than being handed from one to the next. That is the right trade at this size, because the table is short and the alternative is state the search has to carry, and it is a thing to revisit when a table is long rather than a thing to fix now.

The sixth and seventh rows price routing a `return`, and 17.59 ns is the largest number in this table that is ours rather than the language's. It is eight instructions, a `move` into the payload, the `load_int` and jump that enter the body, and a dispatch of `jump_if_false`, `load_int`, `strict_equal` and `jump_if_true` before the `return`. The `strict_equal` is the part worth attacking. It goes through an inline cache and a general comparison in order to ask whether a small integer we put in a register one instruction ago equals a small integer constant, which is a question a compare against an immediate could answer without either. That opcode does not exist yet because nothing needed it, and this is the first thing that does.

The eighth row prices unwinding. Three frames of distance adds 35.18 ns, and three calls measured on the same machine in the same session cost 33.30 ns of that, so popping a frame and missing its handler table is about 0.6 ns. A throw is charged for the calls it has to walk back through and very little else, which is the strongest form of the claim the table design was making.

The last row is the one with a number worth acting on. A caught `TypeError` costs 180 ns more than a caught number, and deferring the object to the `catch` only removes part of that. What is left at the throw is the message, which is a `format!` and a `String` before anything knows whether a handler exists, and what is left at the `catch` is interning the name, allocating the message on the heap and building a two property object with a shape. The first half is the one to attack, because it is paid by every engine error including the ones nothing catches, and making the message a closure over what it needs rather than a built string would move all of it behind the same test that already defers the object. That is exactly the shape of the bug 5.3.6 found in property reads and it is worth writing down before it is fixed rather than after.

## 5.4 Our own stack

JavaScript frames live on a contiguous stack that we allocate and manage, not on the Rust call stack.

Three reasons, all of them load bearing.

**Precise root scanning.** The collector needs to find every live reference. If frames are ours, with a known layout and a known frame size from the blueprint, root scanning is a walk down a contiguous region reading known slots. No conservative stack scanning, no `setjmp` tricks, no registers to spill and guess about. That keeps document 08's collector binding simple and makes moving collection safe.

**Deep recursion does not blow the Rust stack.** A JavaScript program that recurses 50,000 deep is a `RangeError` in Node, not a segfault, and we get to choose our own limit rather than inheriting the OS thread stack.

**Generators, OSR and deopt all need to construct frames from outside.** Resuming a generator is writing a frame. On stack replacement is rewriting a frame in place. Deoptimization is materializing an interpreter frame from optimized state. All three are straightforward with our own stack and horrible with the Rust stack.

Frame layout:

```
[ caller saved pc ] [ caller frame pointer ] [ callee blueprint ] [ context ]
[ this ] [ arg0 .. argN ] [ reg0 .. regM ]
```

Fixed prologue, then arguments, then registers. Frame size is known from the blueprint, so pushing a frame is a bounds check and a pointer bump.

### 5.4.1 The stack, as built

`crates/katsu-vm/src/stack.rs` reserves eight megabytes of address space at startup and commits sixty four kilobytes of it. Reserving is free and committing is what the memory budget in 02.3 counts, so an isolate that never calls anything pays for one chunk rather than for the depth it could have reached. Growing commits another chunk, and shrinking does not hand anything back, because a program that recursed once usually recurses again and the syscall costs more than the page.

The frame header is not inline in the region. That is a deviation from the layout drawn above and the reason is the first of the three arguments for having our own stack at all. With the header inline, every slot in the region is a value except the ones that are not, and the root scanner has to walk frame by frame, read each blueprint to learn its frame size, and skip the right number of words at the right offsets. Off by one there means either tracing a saved program counter as though it were a pointer or missing a live object, and both of those surface as a crash somewhere else entirely. With the header in its own vector, the root set is a slice and there is nothing left to get wrong. The cost is a second allocation and a second cache line touched per call, which is measured below rather than waved at, and if it ever outweighs the safety the header can move back and the tests will not change.

The header holds six numbers now rather than four. Three of them describe the frame, which are where its register zero lives in the region, how many slots it owns and which function in the loaded unit it is running, and one is the environment it reads captured variables through. The other two describe the caller, which are the instruction to resume at and the register the returned value goes into. Keeping the caller's two on the callee is what makes a return a pop and a pair of reads rather than a search, and keeping the function index rather than a pointer to the blueprint is what keeps a frame from borrowing the unit the caller owns.

A pushed frame is fully initialised: the arguments are copied into the registers the calling convention names, and every slot above them is set to `undefined`. That is not only about a read reaching a slot before a write does. A slot still holding the dead frame's pointer is an object the collector would keep alive, and once the collector moves things it is a pointer that gets followed after the object it named has gone. Popping does not clear anything, because the slots stop being roots the moment the top moves down and the next push overwrites all of them.

There are two ways to push, and the difference between them is where the arguments come from. The embedder calling in passes a slice from outside the region. A call inside the program passes a run of the caller's own registers, and those are copied from one part of the region straight into another without ever being gathered into a vector on the way, which is exactly what 4.5 makes the register allocator put arguments in consecutive registers for. The count the callee declares and the count the call site passed are kept apart and the smaller one wins: too few arguments leaves the rest `undefined`, and too many drops the extras, because the registers above the parameters are the callee's scratch and writing an argument into one would corrupt a temporary. The extras are not gone in principle, they are what `arguments` and a rest parameter read, and neither of those exists yet.

The depth limit is ten thousand frames, which is roughly where Node raises `RangeError: Maximum call stack size exceeded` on a default thread stack. A program written against Node that recurses to just under its limit should not fail here, and one that runs away should fail at a comparable point rather than after eating a hundred times the memory. Running out of the reservation raises the same error. Both are ordinary errors and not panics, because a JavaScript program is allowed to catch this one and carry on.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Push and pop a frame | 5.73 ns | 7.99 ns | 7.96 ns |
| Push and pop with three arguments from outside | 7.51 ns | 7.85 ns | 8.27 ns |
| A call and its return, arguments from the caller's registers | 7.70 ns | 8.21 ns | 8.03 ns |
| A hundred frames down and back, per frame | 8.51 ns | 5.52 ns | 5.70 ns |
| Read a register | 0.46 ns | 0.33 ns | 0.35 ns |
| Write a register | 0.57 ns | 0.41 ns | 0.46 ns |
| Two reads and a write | 1.06 ns | 0.77 ns | 0.85 ns |
| Scan eight hundred root slots | 76.1 ns | 256 ns | 258 ns |

Measured on the three reference machines from document 15.5, with eight slot frames because that is roughly what lowering produces for a small function.

A call costs eight nanoseconds of stack work on every machine, which puts the second cache line the split costs somewhere under a nanosecond and settles the question the module doc raises about itself. Two reads and a write is what a three address instruction pays before it does anything, and at around one nanosecond it is a few cycles, which is the floor a register machine is supposed to have.

The recursion row is a hundred real calls with three arguments each before anything is popped, which is not what it measured before calls existed, so it is a new number rather than a changed one. On the two x86 machines it is cheaper per frame than a single push and pop in a loop, at 5.5 against 8.0 nanoseconds, because a loop that pops what it just pushed makes every iteration wait for the one before it while a hundred pushes in a row do not. On the m4 it is the other way round, at 8.5 against 5.7, which is the argument copy showing up a hundred times rather than once.

The first row got 24 percent slower when calls landed, from 6.40 ns to 7.99 ns on the pinned Linux machine, and the cause is worth writing down because it is the same shape as the three in 5.3.2. It is not the code in `push`. The frame header grew from sixteen bytes to twenty four when it gained the function index and the context, and that is the whole difference: applying only those two fields to the previous commit, with nothing else changed, reproduces the regression exactly, at 7.71 ns. Padding the header out to thirty two bytes, which restores the power of two element size a `Vec` indexes with a shift rather than a multiply, does not help, so it is the extra store and the load that reads across it and not the indexing. The same change costs nothing measurable on the m4, where sixteen and twenty four byte headers are within noise of each other. The way back down to sixteen bytes exists and is not free: a `u32` base rather than a `usize`, `size` read from the callee's blueprint instead of stored, and `return_to` recovered by decoding the call instruction at the return address. That trades a store on every call for two loads on every return, so it needs measuring rather than assuming, and it is not worth doing before the frame stops changing shape.

The argument copy costs two nanoseconds on the m4 and nothing on the two x86 machines, which is consistent across reruns and is how the two chips handle a short copy rather than noise. Copying the arguments out of the caller's own registers, which is what a real call does, is no more expensive than copying them in from outside, so the calling convention in 4.5 is getting what it was designed for.

The root scan is the one place the M4 is not merely competitive. Walking eight hundred slots takes 78.6 ns there against 253 ns on a pinned performance core of a 13900K, which is over three times faster on a loop that is nothing but a linear read and a tag test. That is a memory bandwidth result rather than a code result and it does not generalise to anything else in this table, but it is worth writing down, because root scanning is a cost the collector pays on every cycle and it says the machines will disagree about collector tuning later.

## 5.5 Quickening and inline caches in the interpreter

An opcode rewrites itself in place after it learns what it is dealing with. `Add` becomes `Add.Int32` once a site has only seen integers, with a guard that falls back to the generic form and rewrites again to `Add.Generic` if the guard fails twice. `GetProp` becomes `GetProp.Mono` carrying a shape identifier and a slot offset in its inline cache.

This is the highest value single optimization in an interpreter, it is one of the optimizations Deegen applies automatically, and it is the reason the DSL in 5.2 has to enumerate fast paths explicitly rather than hiding them in a generic helper.

Inline cache storage lives in a feedback vector attached to the function, indexed by the `ic` operand, so a cache lookup is one load at a known offset rather than a hash probe.

Under the memory budget from document 02.3, the feedback vector is allocated on the first execution of the first site that needs it, not on function entry. A function that is called once and touches three sites allocates a three entry vector, not a full one. A function that is defined and never called allocates nothing.

The same feedback vector is read by tier 1, tier 2 and the AOT profiler. It is shared state, not per tier state, which is what makes tier up cheap.

## 5.6 Safepoints and interrupts

The interpreter checks an interrupt flag at every loop back edge and every function entry. That single check covers garbage collection safepoints, the tier up counters, execution timeouts, and worker termination.

The check is a load and a compare against a word in the isolate, and the flag is set by the collector or the watchdog. Costing one predictable load per back edge is worth it for having exactly one mechanism instead of four.

## 5.7 What the interpreter is allowed to be

Slow relative to the JIT, and clean. Document 02 already establishes that an interpreter only runtime sits roughly 10x behind Node on compute, and no amount of interpreter tuning closes that. The interpreter's job is to be fast enough that startup and cold code are excellent, correct enough to be the reference implementation, and simple enough that the differential fuzzer's disagreements are always the JIT's fault.

Concretely, the target is to land between QuickJS and V8's jitless mode on the interpreter benchmarks in document 15, and to beat both on startup and memory. Deegen's Lua interpreter beating LuaJIT's assembly interpreter says the generated approach can be genuinely fast, and if we land there for JavaScript that is a publishable result on its own.
