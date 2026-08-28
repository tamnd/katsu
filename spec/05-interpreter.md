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
