# katsu

A JavaScript and TypeScript runtime written in Rust. Its own JIT, built from scratch, no V8 and no JavaScriptCore. An ahead of time mode that compiles your program into a Rust binary. Enough Node.js compatibility that unmodified npm packages run. Two way interop with Rust that is pleasant from both sides.

**Status: M1 in progress, 0.1.4 cut.** `katsu run` executes a program and `console.log` prints, which is the smallest thing a runtime can do that is worth measuring, and the conformance runner reports 6.66% of test262. That number is 5,410 cases of the 81,225 it attempted, and it is small for one specific reason worth knowing before you read anything into it: 75,807 of the cases it could not run stop on the same line of the suite's own harness, which is `throw new Test262Error(message)`, so most of the suite is not being attempted rather than being failed. The wall used to be the `try` on that line and before that the `switch` above it, and it has now moved along the same line to the `new`, which is what the shape of the number means: each statement implemented moves it by one construct rather than knocking it down, and the one left needs constructors and therefore prototypes. Exceptions run now, so a program can `throw`, a `try` can catch across as many frames as it likes, and an engine `TypeError` is a value a handler can read. `finally` runs too, on all five ways out of the block it guards, which means a `return` or a `break` written inside one goes through it and a `return` written in the body wins over the one it interrupted. It added no opcode, because the five ways out are a number in a register and the interpreter already knew how to compare one. Strict mode takes names away as well as changing behaviour, so `"use strict"; var public = 1;` and `"use strict"; eval = 42;` are now refused before anything runs, in node's exact words and pointing at the word rather than the statement. `do while` and the C style `for` join `while`, so every counting loop anyone writes runs, and a `continue` lands in the right place in each of the three, which is the update in a `for` and the test in a `do while`. One thing about them is knowingly wrong and worth saying plainly: a `let` in a `for` head gets one binding per call rather than one per iteration, so a closure made in each iteration sees the final value, and fixing it needs block level environments rather than anything to do with loops. Labels run too, so a `break` or a `continue` can name a loop several levels out instead of leaving only the nearest one, a labelled block is an early exit out of straight line code, and a labelled jump on its way out through a `finally` still runs the `finally` and still lands where it was aimed. The differential harness agrees with node on seven thousand generated programs, after fixing the four bugs it found. Objects have shapes and a literal to build one from, so `{a: 1}` runs, an object can grow a property, and two objects built the same way share one description of their layout. Prototype chains work now, so a property that is not on an object is looked for above it, all the way up rather than one step, and `Object.create` and `Object.getPrototypeOf` are there to build a chain and to read one. A write always makes an own property and leaves the prototype alone, because there are no setters yet for it to reach. The prototype is kept in the shape rather than in the object, and that is the decision the rest of the object model is aimed at: an inline cache that has compared one shape has in that single comparison also checked every prototype between the object and wherever the property was found, so an inherited property is guarded exactly as cheaply as an own one. It is what V8 does with the map and what JavaScriptCore does with the structure, for the same reason in all three places. Properties carry attributes now, so a property knows whether it can be written, whether it shows up when the object is enumerated and whether it can be redefined, and `Object.defineProperty`, `Object.defineProperties` and `Object.getOwnPropertyDescriptor` are there to set and read them. The flags live in the shape next to the prototype, on the node that added the property, and adding `x` as a plain property and adding `x` as a hidden one are two different edges out of the same shape, because two objects that differ in what a `for in` sees are not two objects with one layout. It cost nothing: a shape asked for 28 bytes and the heap reserved 32 for alignment, so the flags went into padding that was already being paid for. Defining a property is not the same operation as assigning to one and the differences are the whole point: `o.x = 1` makes a property that can do all three things and `Object.defineProperty(o, 'x', {value: 1})` makes one that can do none of them, a definition can change what a property is allowed to do and an assignment cannot, and a read only property refuses a write in strict mode and swallows it otherwise. A read only property on a prototype stops a write to every object below it, even though none of those objects has the property, because the chain is searched before the write and what is found there decides the answer. That search made stores faster rather than slower, by about a tenth, because the write now goes straight to the slot the search returned instead of looking the name up a second time. The rules for redefining were measured against node case by case rather than remembered, so a non configurable property can only ever become less permissive, a non writable one can be redefined to the same value and not to a different one, and the comparison is `SameValue`, which is why a non writable `NaN` can be redefined to `NaN` and a positive zero cannot be redefined to a negative one. The statics on `Object` and `JSON` are hidden the way every namespace object in the language hides them, so `console.log(JSON)` prints an empty object and `JSON.stringify(JSON)` is `{}` rather than a listing of the standard library. `this` is bound at call sites now, so a method call knows the object the method was read from and carries it onto the frame the call pushes. That is why `CallMethod` is one opcode rather than a property read followed by a call: the two are kept together so that the receiver is not lost between them. A plain call supplies nothing, and nothing supplied is not the same as `undefined` supplied. That third state is answered inside the callee rather than at the call site, because the answer depends on where the code is and whether it is strict, and neither of those is a question the caller can answer. A strict function called on nothing gets `undefined`. The other two cases refuse by name rather than guess: a sloppy function called on nothing gets `globalThis` and there is no global object yet, and `this` at the top level of a file is `module.exports` and there is no module system yet. Refusing is better than answering `undefined` in both, because `undefined` is a legal answer that a program will act on and a refusal is not. Carrying that third state costs nothing, and that was measured rather than assumed: writing it as an `Option<Value>` took the frame header from 24 bytes to 40, because a NaN boxed value has no spare bit pattern for a discriminant to hide in, and it cost 14 percent on `call/call_return` and on `call/fib`. Encoding it as the empty value instead says the same thing in the eight bytes the value was going to take anyway, and keeps the header at 32. `Object.prototype` has methods on it now, which is what receivers were blocking: `hasOwnProperty`, `isPrototypeOf`, `propertyIsEnumerable`, `toString` and `valueOf`. All five are non enumerable, writable and configurable, which is what node reports for each one, and that is not a detail to skip over, because they sit at the top of nearly every prototype chain in the realm and an enumerable one would appear in every `for in` over every object in the program. Every one of them begins with `ToObject(this)`, and that step has three outcomes here: `undefined` and `null` throw in node's exact words, an ordinary object is itself, and anything else needs a wrapper prototype and so refuses by name rather than answering about a box that was never made. Accessors are here, so a property slot can hold a pair of functions instead of a value and a read or a write on it is a call. The pair is one boxed heap object and not two slots, because a property is one slot everywhere in the heap and making it sometimes two would put a width question on the plain property path that buys nothing there. The fourth attribute bit that says which kind a property is went into the same shape padding the first three did, so it cost nothing again. The receiver is where the access started and not where the property was found, which is the whole reason to put an accessor on a prototype: one getter above answers for every object below it and each of them sees itself as `this`. A setter is the odd case, because its answer is thrown away and the operands of the store that called it are still live, so a setter returns into a register that is nowhere, a sentinel rather than an `Option` for the same reason the receiver is one. The rules were measured against node case by case rather than remembered: printing shows `[Getter]`, `[Setter]`, `[Getter/Setter]` or `undefined` when a property has neither half, a second `defineProperty` keeps the half it does not mention, an accessor turned into a data property comes out not writable with its other two flags intact, and writing to something that has only a getter is swallowed in sloppy mode and refused in strict with node's exact wording. The syntax is here too, so `get x() {}` and `set x(v) {}` in an object literal build one, and two halves written under the same name join into one property rather than replacing each other, in either order and with other properties in between. That turned up something else that was wrong and is now fixed: every property in a literal is defined and not assigned, which is what the language has always said and what katsu was not doing. A definition does not ask the prototype chain for permission, so a setter installed on `Object.prototype` no longer runs when a literal happens to use the same name, a non writable property up the chain no longer stops a literal from having its own, and a literal can now put a value over an accessor the way `{get x() {}, x: 5}` has to. It made literals faster rather than slower, which is the direction that follows from skipping a prototype walk: a two property literal and a four property one are both about six percent quicker over twenty alternating pairs, measured against the benchmarks in the same group that the change cannot touch, and building the same four property object out of four assignments is unchanged because those are still assignments. What is still missing is the other direction, which is that a builtin written in Rust cannot call a JavaScript function, because only the interpreter loop can push a frame, and every place that would need to says so by name rather than answering wrong. They cost a fifth of a property read, and where that went was measured rather than guessed: `property/prop_load` is about a fifth slower over three separate runs of twenty four alternating pairs each, against a control in the same pairs that stayed at 1.02, and swapping the one line that reads the flags back to the lookup that does not put it at 1.00. That is the honest price of the question rather than an implementation slip. A slot can hold a value or a pair of functions and nothing about the slot says which, so something has to be read that says, and the flags on the shape are the only thing that can. Two ways of making it cheaper were tried and neither moved it: shrinking what a lookup returns, and packing the flags into the count word so the search reads one word instead of two, which moved a little cost from reads to writes and left the total alone. The inline cache takes it back completely rather than partly, because a cache that has matched a shape has established in that same comparison that the slot holds a plain value, and it is the next thing on M1. `String({})` still works, because the rule is whether the chain reaches `Object.prototype` rather than whether it finds a method there, and `String(Object.create(null))` throws the same `TypeError` node throws. An object with no prototype at all prints as `[Object: null prototype]`, with the tag counting towards the line width the way node counts it. There is still no computed key and no `delete`, and `typeof Object` says `"object"` here where node says `"function"`, because a function written in Rust is not yet an ordinary object that can carry `create` and `prototype`. A program can also now time itself, because `performance.now()` and `performance.timeOrigin` are there, which is the first piece of the standard library that exists for the benchmark harness rather than for a person reading output. It is monotonic and it is in milliseconds and it counts from the first line of `main`, so it includes our own startup rather than excluding it, and what it says about that is worth writing down: katsu reaches the first line of a program 0.32 ms in, where node takes 19.16 ms to reach the same point. `String()` and `JSON.stringify()` are there as well, and those two were the last names the `fib` workload in [tamnd/katsu-bench](https://github.com/tamnd/katsu-bench) was waiting on, so this project now publishes a compute number about itself for the first time. It is not a good one and it is worth stating plainly rather than burying: fib(35) is 821 ms under katsu against 49.5 ms under node, 34.2 ms under bun and 51.3 ms under deno, medians of five runs interleaved in one session on the same M4, which puts us 16.6 times slower than node and 24 times slower than bun. The absolute has come down from 1,055 ms since 0.1.3 and the ratio has not moved, and the second half of that is the real one: node measured 63 ms in the earlier session and 49.5 ms in this one, the same 21 percent, so what changed between them is how busy the laptop was rather than how fast the interpreter is. That is why the ratio is the column to read and the absolute is not. That is what a bytecode interpreter with no inline caches, no shape guards and no JIT looks like, and none of those three exist yet. The number is here because the 10x goal has to be measured from a real starting point rather than from nothing, and because a baseline nobody publishes is a baseline nobody is held to. One of the six compute workloads runs, up from none. `JSON.parse` is present and refuses by name rather than being absent, because it needs arrays and a missing method would read as a bug in the calling program. There are no modules and no event loop yet. If you want to know what the finished thing is supposed to be and why, read [`spec/`](spec/). If you want to know what is actually built, read the [milestones](https://github.com/tamnd/katsu/milestones).

## The goal

10x faster than Node.js and 10x less memory.

That is the point of the project and it is also the thing most likely to be misunderstood, so here is the honest version up front, broken into the seven axes it actually consists of.

| Axis | Target | Is 10x reachable |
|---|---|---|
| Cold start | 10x to 50x | Yes, and other people have already demonstrated it |
| Baseline memory at idle | Under 4 MiB resident | Yes |
| Distribution size | Under 15 MB against Node's 110 to 160 MB | Yes |
| Typed compute, ahead of time mode | 10x to 100x on typed kernels | Yes, on typed code, and Static Hermes has published numbers |
| Server request throughput | 2x to 4x | No |
| Peak compute on untyped JavaScript | Within 2x of V8, then close it | No, and nobody can |
| Heap size holding millions of live objects | 1.5x to 2x | No |

Four of seven axes support the 10x claim. Three do not. A project that ships a benchmark chart with an asterisk survives, and a project that claims 10x on everything gets taken apart in an afternoon, so the asterisk is in the README rather than in a footnote. [`spec/02-the-10x-goal.md`](spec/02-the-10x-goal.md) has the arithmetic behind every row, and it is the document to read first, because it is the one that decides whether this project is honest.

Every number we publish will name the machine, the operating system, the exact versions compared, the workload, and the run count, and it will be reproducible from a command in [tamnd/katsu-bench](https://github.com/tamnd/katsu-bench). Losses get published next to wins with the same prominence.

## Why this is worth attempting now

Every new JavaScript runtime of the last decade is a new host around one of four twenty year old C++ engines. Deno wraps V8, Bun wraps JavaScriptCore, workerd wraps V8, LLRT wraps QuickJS. The engine layer was assumed closed because building one was assumed to cost a decade.

Three results published between 2021 and 2026 changed that arithmetic. Copy and patch compilation turns a baseline JIT from a hand written assembler into a build step. Deegen generates the interpreter, the baseline JIT and the tier switching logic from one description of the bytecode semantics, and the resulting Lua interpreter beat LuaJIT's hand written assembly interpreter by 1.31x while the generated baseline JIT landed within 33% of LuaJIT's optimizing JIT. MMTk and Whippet turn a production garbage collector from a research project into a dependency.

Add the fact that the CVE history of JavaScript engines is overwhelmingly memory corruption in the JIT and the object model, which is precisely the code Rust makes safer, and there is a real project here that nobody has taken a serious run at.

## What is here

```
crates/
  katsu              the CLI binary
  katsu-runtime      the umbrella facade: what AOT output and embedders link
  katsu-api          the embedding API, the public surface
  katsu-node         the Node compatible layer
  katsu-builtins     ECMAScript builtins
  katsu-aot          the Rust emitter
  katsu-jit          tiers 1 and 2
  katsu-loop         event loop and I/O
  katsu-vm           interpreter, object model, isolates
  katsu-gc           collector interface and binding
  katsu-ir           bytecode, blueprints, the opcode description language
  katsu-parse        frontend: oxc adapter, scopes, lowering
  katsu-platform     OS specifics, W^X, mmap, signals
  katsu-macros       #[katsu::export] and the derives
  katsu-stencils     build time stencil generation and the shipped artifacts
tools/               test262 and differential testing harnesses
xtask/               the architectural rules that are enforced rather than documented
spec/                eighteen documents describing the whole design
```

Dependencies point strictly downward through that stack. `cargo run -p xtask -- layers` fails the build on an upward edge, because every architectural rule that is not mechanically enforced gets violated within a year.

## Building

Stable Rust 1.98 or newer. No nightly toolchain is required and none ever will be for a default build.

```
cargo build --release
cargo test --workspace
cargo run -p xtask -- layers
cargo run -p katsu -- --build-info
```

Conformance needs the suite, which is not vendored because it is a gigabyte of somebody else's repository. Clone it and run the ratchet, which compares against the checked in expectations file and fails on any difference in either direction.

```
git clone --filter=blob:none https://github.com/tc39/test262 vendor/test262
cargo run --release -p test262-runner
cargo run --release -p test262-runner -- --filter language/asi --top 30
```

A run that legitimately changes the result is blessed with `--bless` and the regenerated file goes in the same pull request as the change that caused it. The suite revision is pinned in CI and recorded in the expectations file, because the suite gains and renames tests every week and a run against a different revision produces a diff that looks exactly like a regression.

The differential harness generates programs from a seed and compares what katsu does with them against what node does, which is a different question from conformance: test262 asks whether we follow the standard and this asks whether we behave like the implementation everybody's code was written against. It needs node on the path and says so rather than quietly comparing katsu with itself and reporting that everything agreed.

```
cargo run --release -p differential
cargo run --release -p differential -- --count 50000 --seed 12345
cargo run --release -p differential -- --only 449
```

Every divergence prints the program shrunk down to the statements that still reproduce it and the seed to get it back. Its first run found three bugs: `undefined` was not a binding, `console.log(-0)` printed `0`, and the short circuiting operators built their result in the destination register before evaluating the right hand side, so `v = 1.5 && ('x' + v)` read back the value it had just written. A longer run found a fourth: `9007199254740993 / 10` printed `900719925474099.3` where node prints `900719925474099.2`, because when two shortest forms are equally close to the value the standard takes the even one and Rust takes the larger one. [`spec/14-quality-bar.md`](spec/14-quality-bar.md) section 14.5.1 has the detail.

Benchmarks run on the reference machines named in [`spec/15-benchmarks.md`](spec/15-benchmarks.md) rather than on whatever laptop is nearest, because a timing without a machine attached to it is not a result. `cargo run -p xtask -- machines` lists them and says which ones are reachable, and `cargo run -p xtask -- bench --machine gamingpc -p katsu-vm` runs a crate's benchmarks on the x86-64 reference by checking out the current commit there, which it refuses to do if that commit has not been pushed.

## The design, in one screen

**Our own engine.** Not a wrapper, not a fork. This is the project and it is why the timeline is measured in years.

**Three tiers, and tier 0 is an interpreter we take seriously.** Interpreter, baseline JIT, optimizing JIT. Most JavaScript in a real process runs a handful of times and never gets hot, so the interpreter gets inline caches, quickening and a register bytecode instead of being a placeholder.

**The baseline JIT is generated from the interpreter, not written by hand.** Copy and patch, following Deegen and Druid. One semantic description produces both tiers, so they cannot drift apart, and code generation costs microseconds. This is the highest leverage decision in the design and also the riskiest, which is why it is gated behind a spike at M2 before anything is built on top of it.

**The optimizing tier uses a control flow graph SSA IR, not sea of nodes.** V8 spent three years moving off sea of nodes to Turboshaft and reported compile time roughly halved with equal or better code quality. We start where they ended up.

**Pointer compression from day one.** Object slots are 4 bytes. V8 measured up to 43% heap reduction from this. It caps one isolate at 4 GB of heap, which is the same trade Chrome made.

**A collector from the research literature, not a hand rolled one.** MMTk is the default choice with Immix as the plan, and Whippet is the fallback. The decision is deferred to a measurement at M4 rather than made in advance.

**TypeScript types are erased at runtime and treated as hints, never facts, in ahead of time mode.** TypeScript is deliberately unsound. Static Hermes gets its enormous numbers by requiring sound types, and we cannot require that of an ecosystem that does not have it, so we specialize on types and guard anyway.

**Ahead of time mode emits a Rust crate that links the same runtime.** Your class does not become a `struct`. The emitted Rust contains the operations the optimizing JIT would have emitted, against the same object model, so the semantics are the real ones. Ahead of time binaries embed the interpreter as the deoptimization target, because without one they are either conservative and slow or fast and wrong.

**Our own event loop on tokio, thread per core, epoll and kqueue by default.** Node's observable phase ordering is the contract, libuv is not. io_uring is an opt in accelerator rather than the default, because Docker 25 blocks its syscalls in the default seccomp profile and software that requires it simply fails to start in a normal container.

**Node-API addons yes, raw V8 API addons not before 1.0.**

**Linux, macOS and Windows, x86-64 and aarch64, at parity.** Every platform is tested on every commit and a failure on any of them blocks a merge, because a platform that is only checked before a release is a platform that is broken most of the time.

## What this is not

Not a browser engine. No DOM, no layout. Web APIs only where the WinterTC Minimum Common API, now ECMA-429, says a server runtime should have them.

Not a type checker. `katsu run` strips types like everyone else. `katsu check` shells out to the TypeScript compiler.

Not a package manager. We read `node_modules`. Use npm, pnpm or bun to create it.

Not finished, and not close. [`spec/13-milestones.md`](spec/13-milestones.md) has thirteen milestones, estimates the total at 55 to 90 engineer-months, and names four points where stopping still leaves a real product rather than a sunk cost.

## Related repositories

[tamnd/katsu-compat](https://github.com/tamnd/katsu-compat) runs Node.js's own test suite and a corpus of real npm packages against katsu, and publishes the pass rate per module with every failure listed.

[tamnd/katsu-bench](https://github.com/tamnd/katsu-bench) measures katsu against Node.js, Bun and Deno on every axis in the table above, and publishes the losses too.

[tamnd/bento](https://github.com/tamnd/bento) is the same idea in Go, and is where a lot of the compatibility thinking came from.

## On the name

カツ, the panko fried cutlet, and 勝つ, to win. They are homophones, and the pun is live enough in Japan that students eat katsudon the night before an exam for exactly that reason. A runtime whose entire thesis is a performance claim may as well be named "to win".

It also sits in the family properly. Bun is bread. Bento is the boxed meal. Katsu is what goes in the box.

## License

MIT or Apache-2.0, at your option.
