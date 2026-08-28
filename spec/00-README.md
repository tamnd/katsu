# Spec 2128: katsu

A JavaScript and TypeScript runtime written in Rust. Its own JIT, built from scratch, no V8 and no JavaScriptCore. An ahead of time mode that compiles your program into a Rust binary. Enough Node.js compatibility that unmodified npm packages run. Two way interop with Rust that is pleasant from both sides.

Repo: `github.com/tamnd/katsu`. Binary: `katsu`. Written 28 August 2026.

## The goal

10x faster than Node.js and 10x less memory.

That target is the point of the project and it is also the thing most likely to be misunderstood, so document 02 exists to break it into seven separate axes and say, for each one, whether 10x is physically reachable and what it would take. The short version, with the evidence in document 01:

Startup and cold start: 10x to 50x is real and other people have already demonstrated it. Baseline memory: 10x is real. Distribution size: 10x is real, since Node ships 110 to 160MB and we are targeting under 15. Typed compute in AOT mode: 10x to 100x is real on typed code, and Static Hermes has published the numbers. Server request throughput: 2x to 4x is realistic. Peak throughput on ordinary untyped JavaScript: 10x is not reachable by anyone, because V8's TurboFan is already close to the ceiling for those semantics, and our honest target there is to get within a factor of two of V8 and then close it. Heap size for a workload holding millions of live objects: 1.5x to 2x, because V8 already does pointer compression and the object graph is the object graph.

Four of seven axes support the 10x claim. Three do not, and document 02 says so in the same words we will use in the README of the actual repo. A project that ships a benchmark chart with an asterisk survives. A project that claims 10x on everything gets taken apart on Hacker News in an afternoon.

## Why now

Every "new" JavaScript runtime of the last decade is a new host around one of four twenty year old C++ engines. Deno wraps V8, Bun wraps JavaScriptCore, workerd wraps V8, LLRT wraps QuickJS. The engine layer was assumed closed because building one was assumed to cost a decade.

Three results published between 2021 and 2026 changed that arithmetic.

Copy and patch compilation (Xu and Kjolstad, OOPSLA 2021) turns a baseline JIT from a hand written assembler into a build step. Deegen (Xu and Kjolstad, OOPSLA 2025) generates the interpreter, the baseline JIT and the tier switching logic from one description of the bytecode semantics, and the resulting Lua interpreter beat LuaJIT's hand written assembly interpreter by 1.31x while the generated baseline JIT landed within 33% of LuaJIT's optimizing JIT. MMTk and Whippet turn a production garbage collector from a research project into a dependency.

Add the fact that the CVE history of JavaScript engines is overwhelmingly memory corruption in the JIT and the object model, which is precisely the code Rust makes safer, and there is a real project here that nobody has taken a serious run at.

## What "100% Node compatible" means

Take an unmodified npm package with a normal dependency tree, run `katsu run`, and it works. Same CommonJS and ESM semantics, same `node_modules` resolution, same `node:*` modules, same `process`, same streams, same event loop phase ordering, same `Buffer`.

It does not mean binary compatibility with V8 internals. Addons written against Node-API work, because Node-API is ABI stable by design and is what napi-rs, swc, rolldown, lightningcss and most modern native packages already use. Addons that reach into V8's C++ classes directly do not work out of the box. Bun has shown that a V8 API shim on a non-V8 engine is possible, and their own writeup shows how ugly it gets: they had to emulate V8's memory layout for inlined functions like `GetInternalField` that do pointer arithmetic in the header. Document 10 treats that as a post 1.0 possibility, not a promise.

It also does not mean bug for bug fidelity with V8's non standard behaviour. Document 10 lists the known divergences instead of quietly missing them.

## The documents

| | | |
|---|---|---|
| 00 | this file | the pitch, the settled decisions, what to read first |
| 01 | `01-research-2026.md` | the verified landscape, the papers, the numbers, with sources |
| 02 | `02-the-10x-goal.md` | the seven axes, what is reachable on each, what each one forces |
| 03 | `03-architecture.md` | layers, crates, how a program flows through both modes |
| 04 | `04-frontend.md` | parsing, TypeScript, module resolution, lowering to bytecode |
| 05 | `05-interpreter.md` | the bytecode, dispatch without guaranteed tail calls, quickening |
| 06 | `06-jit-tiers.md` | copy and patch baseline, the optimizing tier, OSR, deopt |
| 07 | `07-object-model.md` | values, shapes, inline caches, arrays, strings |
| 08 | `08-gc-and-memory.md` | the collector, barriers, the memory budget line by line |
| 09 | `09-aot-mode.md` | compiling to Rust, what types buy, profile guided AOT |
| 10 | `10-node-compat.md` | the Node surface, addons, the compatibility matrix |
| 11 | `11-rust-interop.md` | both directions, the macro, ownership, async bridging |
| 12 | `12-concurrency-and-io.md` | event loop, io_uring and kqueue, workers, isolates |
| 13 | `13-milestones.md` | M0 to M12, exit criteria, the four sane places to stop |
| 14 | `14-quality-bar.md` | test262, the Node suite, differential fuzzing, JIT security |
| 15 | `15-benchmarks.md` | what we measure, against whom, and the rules for reporting |
| 16 | `16-package-layout.md` | the crate tree and stability tiers |
| 17 | `17-open-questions.md` | the ranked list that has to be answered before M2 |

Read 02 first. It is the document that decides whether this project is honest.

## Decisions already made

**Our own engine.** Not a wrapper, not a fork. This is the project and it is why the timeline is years.

**Three tiers, and tier 0 is an interpreter we take seriously.** Interpreter, baseline JIT, optimizing JIT. Most JavaScript in a real process runs a handful of times and never gets hot, so the interpreter gets inline caches, quickening and a register bytecode instead of being a placeholder. Document 05.

**The baseline JIT is generated from the interpreter, not written by hand.** Copy and patch, following Deegen and Druid. One semantic description produces both tiers, so they cannot drift apart, and code generation costs microseconds. This is the highest leverage decision in the spec. Document 06.

**The optimizing tier uses a CFG based SSA IR, not sea of nodes.** V8 spent three years moving off sea of nodes to Turboshaft and reported compile time roughly halved with equal or better code quality. We start where they ended up. Document 06.

**Pointer compression from day one.** Object slots are 4 bytes. V8 measured up to 43% heap reduction from this and a developer on the team put the cost of disabling it at 60 to 70% more memory. It caps one isolate at 4 GB of heap, which is the same trade Chrome made. Document 07.

**A real collector from the research literature, not a hand rolled one.** MMTk is the default choice with Immix as the plan, and Whippet is the fallback if MMTk's binding overhead does not suit us. Document 08 covers the evaluation and why the decision is deferred to a measurement rather than made here.

**TypeScript types are erased at runtime and treated as hints, never facts, in AOT mode.** TypeScript is deliberately unsound. Static Hermes gets its 300x by requiring sound types; we cannot require that of an ecosystem that does not have it, so we specialize on types and guard anyway. Document 09.

**AOT emits a Rust crate that links the same runtime.** Your class does not become a `struct`. The emitted Rust contains the operations the optimizing JIT would have emitted, against the same object model, so the semantics are the real ones. Document 09 is blunt about this because it is the most misunderstandable thing in the spec.

**AOT binaries embed the interpreter** as the deoptimization target. Without it, AOT is either conservative and slow or fast and wrong.

**Our own event loop on tokio, thread per core, epoll and kqueue by default.** Node's observable phase ordering is the contract, libuv is not. io_uring is an opt in accelerator rather than the default, because Docker 25 blocks its syscalls in the default seccomp profile and software that requires it simply fails to start in a normal container. Document 12.

**Node-API addons yes, V8 API addons not before 1.0.** Document 10.

**Linux and macOS, x86-64 and aarch64, at parity. Windows after 1.0.**

## What this is not

Not a browser engine. No DOM, no layout. Web APIs only where the WinterTC Minimum Common API (now ECMA-429, standardized in Ecma TC55) says a server runtime should have them.

Not a type checker. `katsu run` strips types like everyone else. `katsu check` shells out to TypeScript 7, which shipped as a Go native compiler in July 2026 and is 8 to 12x faster than the old one.

Not a package manager. We read `node_modules`. Use npm, pnpm or bun to create it.

Not finished. Document 13 has thirteen milestones and names four of them as points where stopping still leaves a real product, the first being M4.

## Honesty about scope

Between 55 and 90 engineer-months to 1.0, which is four to seven person-years and does not parallelize well below about four people. The parts most likely to kill it are not the ones that sound hardest.

The optimizing JIT is hard but well trodden. A mediocre optimizing tier still beats no optimizing tier.

Node compatibility is what actually kills projects like this, and it kills them slowly. There is no single hard problem, there are eleven hundred small ones, and the failure mode is a program that works for twenty minutes and then hits a stream edge case nobody documented. Document 10 proposes the only defence that has ever worked, which is running other people's real test suites instead of writing our own.

The riskiest technical assumption is that copy and patch produces good enough baseline code for JavaScript specifically. It is proven for Lua, for SQL, for Wasm and for CPython's bytecode. It is not proven for a language where nearly every operation is a polymorphic dispatch through an inline cache, and Haoran Xu's own writeup notes that naive stencils produce poor code because everything becomes a call unless you restructure into continuation passing style. Document 17 makes this open question one and document 13 puts the experiment in M2, before anything depends on it.

The second riskiest is that the 10x memory goal and the peak throughput goal pull against each other. Inline caches, feedback vectors and compiled code are all memory spent to buy speed. Document 02 sets an explicit budget and document 08 says what we drop when the budget is hit.

## On the name

カツ, the panko fried cutlet, and 勝つ, to win. They are homophones, and the pun is live enough in Japan that students eat katsudon the night before an exam for exactly that reason. A runtime whose entire thesis is a performance claim may as well be named "to win".

It also sits in the family properly. Bun is bread. Bento, our Go runtime, is the boxed meal. Katsu is what goes in the box. The two runtimes end up related rather than being two unconnected Japanese nouns.

Five letters, a hard consonant cluster at each end so it sounds quick, spellable from hearing it once, and a cutlet cross section with the ridged panko edge reads at 16 pixels.

The registry situation, checked against the registries directly rather than guessed:

`crates.io/crates/katsu` is **free**, which is the one that matters, because that is where a Rust project publishes. The top level crate is simply `katsu`.

`npmjs.com/package/katsu` is taken by an abandoned "nodejs content management framework" at version 0.1.0 with eight downloads in the last month. That is a dead squat rather than a package with users, and an npm installer can be scoped to `@katsu/cli` regardless.

One collision worth knowing about: [FyraLabs/katsu](https://github.com/FyraLabs/katsu) is a Rust image builder for Ultramarine Linux. Different domain, not published to crates.io, and not a reason to pick a different name.

The alternates from the naming discussion, all confirmed free on crates.io, were `gyoza`, `ohagi` and `karaage`. `ohagi` is the only one free on both registries. The cheapest moment to change is before M0.
