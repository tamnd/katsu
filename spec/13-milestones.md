# Milestones

## 13.1 How to read this

No dates. Dates on a project of this shape are fiction, and the useful thing is ordering and exit criteria.

Every milestone has an exit criterion that is a measurement or a passing test, not a feeling. Several milestones exist purely to answer a question before the architecture depends on the answer, and those are marked as gates. A gate that fails is not a disaster, it is the milestone working: the whole point of doing the copy and patch spike at M2 is to find out at M2 rather than at M7.

The estimates are in engineer-months for a small senior team, and 13.5 says what that adds up to.

## 13.2 The sequence

| M | Name | Exit criterion | Gate |
|---|---|---|---|
| M0 | Skeleton | `katsu run` prints from a trivial script through the real pipeline | |
| M1 | Language core | test262 above 80%, interpreter only | |
| M2 | Generation and stencils | one opcode description drives interpreter and stencils; measured speedup on property heavy code | yes |
| M3 | Interpreter complete | test262 above 95%; dispatch strategy chosen by measurement | yes |
| M4 | Real GC | collector integrated; MMTk against Whippet decided by measurement | yes |
| M5 | Baseline JIT | tier 1 beats the interpreter by 3x or more; soak test running in CI | |
| M6 | Optimizing JIT prototype | tier 2 on numeric code; Cranelift against our own backend decided | yes |
| M7 | Node core | HTTP server, fs, streams; Express and Fastify run | |
| M8 | Native addons | Node-API host; `better-sqlite3` and `sharp` load and pass their tests | yes |
| M9 | AOT mode | `katsu build` produces a working binary from a real application | |
| M10 | Rust interop | both directions, measured against napi-rs | |
| M11 | Hardening | fuzzing, the security review, the memory budget enforced in CI | |
| M12 | 1.0 | the compatibility and performance numbers in document 02 published and reproducible | |

## 13.3 The milestones in detail

**M0, Skeleton.** oxc parsing into our AST adapter, scope analysis, lowering to bytecode, a `loop { match }` interpreter handling a few dozen opcodes, a bump allocated heap with no collector, and a CLI. The point is that every layer in document 03 exists and the seams are real, so that later work is filling in rather than restructuring. Roughly 2 to 3 engineer-months.

**M1, Language core.** Objects, prototypes, shapes, inline caches in the interpreter, closures, exceptions, classes, the core builtins, generators and async through the frontend state machine transformation, and modules. test262 above 80%. The remaining 20% is where the horrors live and M3 is what pays for them. Roughly 6 to 9 engineer-months, and this is where a team discovers whether it enjoys this kind of work.

**M2, Generation and stencils. Gate.** The opcode DSL from document 05.2 and the generator behind it. The build time stencil extraction from document 06.3, understanding ELF and Mach-O relocations, for x86-64 and aarch64. A prototype tier 1 for a subset of opcodes.

The exit criterion is a number: **generated stencils must beat the interpreter by a worthwhile factor on property heavy code, not just on arithmetic loops.** Document 06.2 explains why this is the risk, and document 17 makes it open question one. If the answer is no, the fallback is a hand written baseline JIT for the top thirty opcodes and a generated one for the rest, which is more work and less elegant but not fatal. Finding this out here is worth the schedule slot. Roughly 4 to 6 engineer-months.

**M3, Interpreter complete. Gate.** test262 above 95%, which means the ugly parts: `with`, direct `eval`, Annex B, the full `RegExp` surface through `regress`, `Proxy` and `Reflect`, `Intl` decisions, labeled continue across finally, and the tagged template cooked and raw edge cases.

The second exit criterion is the dispatch decision from document 05.3, made by benchmarking A against B against C rather than by argument. Roughly 4 to 6 engineer-months.

**M4, Real GC. Gate.** The interface from document 08.2, then integration with the chosen library. The decision criteria are throughput on our own allocation traces, pause distribution, resident memory at a tight heap size, ephemeron support, and the cost of the FFI layer that Whippet requires and MMTk does not.

Also here: the handle and scope API from document 08.4, which every builtin then gets rewritten against, which is why it happens now rather than later. Roughly 3 to 5 engineer-months.

**M5, Baseline JIT.** Full tier 1 across the opcode set, tier up at eight invocations, OSR into tier 1, inline cache slabs, the W^X discipline from document 06.9, and the code memory budget with eviction from document 06.8.

Exit criterion is 3x or better over the interpreter on a benchmark suite that includes real code, plus the long running soak test in CI from here on, because fragmentation and code cache pathologies only show up over hours. Roughly 4 to 6 engineer-months.

**M6, Optimizing JIT prototype. Gate.** The CFG SSA IR, the graph builder from bytecode plus feedback, and enough of the pipeline to compile typed numeric code with guards and deoptimization.

The decision is Cranelift against our own backend, made by building the deopt path on Cranelift's user stack maps and finding out whether it works. Document 06.7 explains why this cannot be decided from documentation. Roughly 6 to 9 engineer-months for the prototype, and the full tier 2 with escape analysis and the rest is another 9 to 12 after it.

**M7, Node core.** The module table's first tier from document 10.9, the event loop with Node's exact phase ordering, and enough of `http`, `net`, `fs`, `stream` and `crypto` to run a real server. Exit criterion is Express and Fastify running their own test suites. Roughly 9 to 12 engineer-months, and this is the milestone most likely to be underestimated because there is no cleverness in it, only surface area.

**M8, Native addons. Gate.** The Node-API host from document 10.5. Exit criterion is that `better-sqlite3` and `sharp` install, load, and pass their own test suites unmodified, because those two exercise most of the interesting API including external buffers, finalizers, and threadsafe functions.

This is the gate on the compatibility claim. Also here: `Error.captureStackTrace` and `prepareStackTrace` working well enough that source map libraries produce correct output. Roughly 4 to 6 engineer-months.

**M9, AOT mode.** The Rust emitter, the type inference from document 09.4, the interpreter embedded as a deopt target, dead code elimination across the module graph, and profile guided builds. Exit criterion is `katsu build` on a real CLI application producing a binary under 15 MB that starts in under a millisecond and behaves identically to the JIT run. Roughly 6 to 9 engineer-months.

**M10, Rust interop.** The export macro, the embedding API, the derive macros, async bridging, and the thread handle. Exit criterion is the numbers in document 11.9 measured against napi-rs on the same machine. Roughly 3 to 5 engineer-months.

**M11, Hardening.** Differential fuzzing between the interpreter and both JIT tiers, structure aware JavaScript fuzzing, the memory budget enforced as a failing CI test, an external security review of the JIT and the cage, and the platform matrix. Roughly 4 to 6 engineer-months and it never actually ends.

**M12, 1.0.** Documentation, the compatibility table, the reproducible benchmark harness from document 15, and the published numbers with the reporting rules from document 02.8 applied.

## 13.4 Four places it is sane to stop

Each of these is a shippable product, and saying so in advance is what stops the project from being all or nothing.

**After M4: a JavaScript interpreter in Rust with a real collector.** Comparable to QuickJS with better memory behavior and a genuinely nice Rust embedding API. There is a real audience for that among people embedding scripting in Rust applications who currently choose between rquickjs and building V8.

**After M7: a fast starting Node compatible runtime with a baseline JIT.** This is the LLRT and Bun-for-serverless niche, and cold start plus memory are already 10x at this point, since those wins come from the architecture rather than from tier 2. This is the first point where the headline claim in document 02 is defensible.

**After M9: the AOT compiler as a product on its own.** `katsu build` competing with Perry and Static Hermes, with the differentiator being full JavaScript semantics with guards rather than a typed subset. Shippable independently of whether tier 2 ever reaches parity.

**After M12: the whole thing.**

## 13.5 What this actually costs

Adding it up, without the optimism that usually gets applied: somewhere between 55 and 90 engineer-months to M12, so three to five years for a team of two, or two to three years for a team of four, with the caveat that this kind of work does not parallelize well below about four people because the pieces are deeply coupled.

That is a real number and it should be read before starting rather than discovered in year two. It is also why 13.4 exists. Every one of those four stopping points is a place where the work done so far is a product rather than a sunk cost, which is the only responsible way to structure a project of this length.

## 13.6 What would make us stop

Stated in advance, because criteria invented after a bad result are not criteria.

If M2 shows that copy and patch cannot produce competitive code for polymorphic JavaScript operations, and the hand written fallback is also unattractive, then the JIT strategy is wrong and the project becomes an AOT compiler plus an interpreter. That is still a product, per 13.4.

If M4 shows that neither collector library can hold our memory budget at acceptable throughput, the memory claim in document 02 has to be revised downward publicly before it is ever made.

If M8 shows that Node-API on a non-V8 engine hits an unfixable wall, the compatibility claim narrows from "runs Node programs" to "runs pure JavaScript Node programs", which is a materially smaller product and should be admitted as such rather than hedged.

If, by M12, tier 2 is more than 3x behind V8 on real workloads rather than the near parity document 02.6 projects, then the honest framing is a runtime that wins on startup, memory and deployment while losing on peak compute. That is still a good product for a large set of users, and it is what the reporting rules in document 02.8 exist to let us say without embarrassment.
