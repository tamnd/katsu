# katsu

A JavaScript and TypeScript runtime written in Rust. Its own JIT, built from scratch, no V8 and no JavaScriptCore. An ahead of time mode that compiles your program into a Rust binary. Enough Node.js compatibility that unmodified npm packages run. Two way interop with Rust that is pleasant from both sides.

**Status: M1 in progress, 0.1.1 cut.** `katsu run` executes a program and `console.log` prints, which is the smallest thing a runtime can do that is worth measuring, and the conformance runner reports 6.66% of test262. That number is 5,410 cases of the 81,225 it attempted, and it is small for one specific reason worth knowing before you read anything into it: 75,807 of the cases it could not run stop on the same line of the suite's own harness, which is `throw new Test262Error(message)`, so most of the suite is not being attempted rather than being failed. The wall used to be the `try` on that line and before that the `switch` above it, and it has now moved along the same line to the `new`, which is what the shape of the number means: each statement implemented moves it by one construct rather than knocking it down, and the one left needs constructors and therefore prototypes. Exceptions run now, so a program can `throw`, a `try` can catch across as many frames as it likes, and an engine `TypeError` is a value a handler can read. `finally` runs too, on all five ways out of the block it guards, which means a `return` or a `break` written inside one goes through it and a `return` written in the body wins over the one it interrupted. It added no opcode, because the five ways out are a number in a register and the interpreter already knew how to compare one. Strict mode takes names away as well as changing behaviour, so `"use strict"; var public = 1;` and `"use strict"; eval = 42;` are now refused before anything runs, in node's exact words and pointing at the word rather than the statement. `do while` and the C style `for` join `while`, so every counting loop anyone writes runs, and a `continue` lands in the right place in each of the three, which is the update in a `for` and the test in a `do while`. One thing about them is knowingly wrong and worth saying plainly: a `let` in a `for` head gets one binding per call rather than one per iteration, so a closure made in each iteration sees the final value, and fixing it needs block level environments rather than anything to do with loops. Labels run too, so a `break` or a `continue` can name a loop several levels out instead of leaving only the nearest one, a labelled block is an early exit out of straight line code, and a labelled jump on its way out through a `finally` still runs the `finally` and still lands where it was aimed. The differential harness agrees with node on seven thousand generated programs, after fixing the four bugs it found. Objects have shapes and a literal to build one from, so `{a: 1}` runs, an object can grow a property, and two objects built the same way share one description of their layout. There is no prototype to look through yet, no computed key and no `delete`. There are no modules and no event loop yet. If you want to know what the finished thing is supposed to be and why, read [`spec/`](spec/). If you want to know what is actually built, read the [milestones](https://github.com/tamnd/katsu/milestones).

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
