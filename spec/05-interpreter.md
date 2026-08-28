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
| Exceptions | `Throw`, `ReThrow`, `PopHandler` |
| Modules | `LdModuleVar`, `StModuleVar`, `ImportDynamic`, `ImportMeta` |
| Type queries | `TypeOf`, `ToNumber`, `ToString`, `ToObject`, `ToPropertyKey` |

Every opcode that can be polymorphic carries an inline cache slot index as an operand. That includes all property access, all calls, all arithmetic and all comparison, because `+` in JavaScript is a polymorphic dispatch just as much as `o.x` is.

The bytecode is versioned and serializable, because it is simultaneously the interpreter's input, the JIT's input, the AOT compiler's input, and the on disk cache format.

### 5.1.1 The bytecode, as built

`crates/katsu-ir` is four modules: the instruction set, the per function constant pool, the source position table, and the blueprint that holds all three. Everything is re-exported at the crate root, because a consumer wants `katsu_ir::Op` and does not care which file it is written in.

What exists is the decoded form, one Rust enum with one variant per opcode. The byte encoding this document specifies, one byte of opcode then operands with a wide prefix past 256 registers, is not built yet, because nothing writes bytecode to disk until the cache lands. The two will exist side by side rather than one replacing the other: the interpreter matches on the decoded form and the encoding is a lowering of it.

Jumps carry an absolute instruction index rather than a signed offset. An offset is smaller and relocatable and is what the encoded form will use. An absolute index is the one that cannot be wrong by a sign, which matters more while the forward reference patch list in lowering is new.

Every opcode in the set has a construct in the M0 subset that lowers to it. The families listed above that M0 has no syntax for, iteration and generators and modules and private names and exception handlers, are deliberately absent. An opcode with no producer is an opcode whose semantics nobody has had to think about, and it sits in the file looking implemented.

Four opcodes exist that the table above does not name. Three are there because a static analysis found something a run time check would otherwise have to find. `LoadUninitialized` writes the hole into a `let` or `const` slot when its scope is entered, and `ThrowIfUninitialized` is the dead zone check, emitted only where scope analysis says a reference can reach one. `ThrowConstAssignment` is the third, and it is an opcode rather than an early error because Node reports assignment to a `const` as a run time TypeError.

`LoadClosure` is the fourth and it is there for a different reason. `const f = function me() { return me; };` binds `me` inside the function and nowhere else, and what it binds is the closure that is running rather than whatever `f` holds by the time the call happens. There is nothing in the frame or the environment to read that from, so it is an opcode, emitted once in the prologue of a named function expression and nowhere else.

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

Everything that happens inside a single frame runs: the loads and moves, arithmetic, the bitwise operators, comparisons, the unary operators, the temporal dead zone checks, jumps, back edges and `return`. Calls, closures, environments, globals and property access all need a heap object with a header to point at, which M0 does not have yet, so every one of them falls through to the arm at the bottom of the match and produces an error naming the opcode. A refusal that says `get_index is not implemented yet` is worth a great deal more than a wrong answer, and it is the difference between a runtime that is incomplete and one that is untrustworthy.

The numeric conversions live in their own module, because each of them is a place where the obvious Rust expression is subtly not what JavaScript says. `ToInt32` is a modulo and a fold rather than a cast, so `1e10 | 0` is `1410065408` while `1e10 as i32` saturates. A shift count is taken modulo thirty two, so `1 << 32` is `1`. Exponentiation disagrees with IEEE `pow` in exactly two places, `1 ** NaN` and `(-1) ** Infinity`, both of which are `NaN` in JavaScript and one in IEEE. Relational comparison is the one case where the obvious expression is right, because Rust's float operators are the IEEE ones and already return false on both sides of a `NaN`, which is what the standard asks for.

Back edges check one shared atomic word, which is the mechanism 5.6 describes, and a test proves an endless loop can be stopped from another thread. That check is the only per iteration cost the loop pays that a straight line of instructions does not.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Move a register | 0.87 ns | 0.78 ns | 0.85 ns |
| Add two numbers | 1.70 ns | 2.83 ns | 2.10 ns |
| Compare two numbers | 1.35 ns | 1.25 ns | 1.49 ns |
| Raise to a power | 3.59 ns | 8.42 ns | 4.90 ns |
| One counting loop iteration | 6.55 ns | 6.43 ns | 6.59 ns |
| The same, per instruction | 1.64 ns | 1.61 ns | 1.65 ns |

Measured on the three reference machines from document 15.5, a thousand instructions to a run so that the frame push either side is noise.

A move is the floor, because it reads one register and writes another and everything left over is the fetch, the bounds check and the branch the match compiles into. Under a nanosecond on all three machines is a good deal better than the folklore about a single dispatch site predicts, and it is the number strategies B and C have to beat.

The interesting result is that an add costs more than a comparison on both x86 machines and about the same on the M4, when the two arms do identical work apart from the operator. The difference is entirely in what happens to the result. A comparison writes a boolean, which is a tag and nothing else. An add writes a number through `Value::from_f64`, which checks whether the double is exactly an integer and re-encodes it as a tagged integer when it is, so that the next instruction to read it gets an integer rather than a boxed double. That check is the right default and it is also the single clearest argument for the integer quickening in 5.5: an `Add.Int32` that stays in integers throughout never converts to a double and never converts back, and the gap between the add row and the compare row is roughly what it stands to win.

Exponentiation is the one operator that calls into libm rather than compiling to an instruction, and it is the only row where the same silicon disagrees with itself. The 13900K runs it in 8.42 ns under WSL2 and 4.90 ns natively, which is glibc's `pow` against the Microsoft runtime's and not anything we wrote. It is worth knowing before anybody reads a benchmark that leans on `Math.pow` and concludes something about the engine.

The counting loop is the row that resembles a real program: a comparison, a conditional jump, an add and a back edge, with a dependency between every pair of them and a branch for the predictor to get right. Six and a half nanoseconds an iteration on all three machines, about 1.6 ns an instruction, which is close enough to the straight line rows to say the branch is predicted and the interrupt check is close to free. No comparison against Node is claimed here, because a fair one needs the differential harness and a program that both runtimes can actually run.

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

A pushed frame is fully initialised: the arguments are copied into the registers the calling convention names, and every slot above them is set to `undefined`. That is not only about a read reaching a slot before a write does. A slot still holding the dead frame's pointer is an object the collector would keep alive, and once the collector moves things it is a pointer that gets followed after the object it named has gone. Popping does not clear anything, because the slots stop being roots the moment the top moves down and the next push overwrites all of them.

The depth limit is ten thousand frames, which is roughly where Node raises `RangeError: Maximum call stack size exceeded` on a default thread stack. A program written against Node that recurses to just under its limit should not fail here, and one that runs away should fail at a comparable point rather than after eating a hundred times the memory. Running out of the reservation raises the same error. Both are ordinary errors and not panics, because a JavaScript program is allowed to catch this one and carry on.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Push and pop a frame | 5.3 ns | 6.3 ns | 6.7 ns |
| Push and pop with three arguments | 7.4 ns | 6.3 ns | 7.0 ns |
| A hundred frames down and back, per frame | 5.1 ns | 4.2 ns | 6.3 ns |
| Read a register | 0.48 ns | 0.34 ns | 0.38 ns |
| Write a register | 0.62 ns | 0.41 ns | 0.54 ns |
| Two reads and a write | 1.06 ns | 0.80 ns | 0.99 ns |
| Scan eight hundred root slots | 78.6 ns | 253 ns | 310 ns |

Measured on the three reference machines from document 15.5, with eight slot frames because that is roughly what lowering produces for a small function.

A call costs six nanoseconds of stack work on every machine, which puts the second cache line the split costs somewhere under a nanosecond and settles the question the module doc raises about itself. Two reads and a write is what a three address instruction pays before it does anything, and at around one nanosecond it is a few cycles, which is the floor a register machine is supposed to have. The argument copy is free on the two x86 machines and costs two nanoseconds on the M4, which is small enough and consistent enough across reruns to be a real difference in how the two chips handle a short unaligned copy rather than noise.

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
