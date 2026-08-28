# The landscape, checked August 2026

Everything in this document was checked against sources during the week of 28 August 2026, and the links are inline. Where a claim is a vendor number or a single blog post rather than a reproducible measurement, it says so. Where something could not be confirmed, it is marked **[unverified]** and document 17 tracks it.

The point of this document is not to survey the field. It is that about fifteen specific facts determine most of the architecture, and when one of them changes we need to know which decisions to revisit.

## 1. The three results that make this project viable

### 1.1 Copy and patch compilation

Xu and Kjolstad, "Copy-and-patch compilation", OOPSLA 2021 ([paper](https://dl.acm.org/doi/10.1145/3485513), [arXiv](https://arxiv.org/pdf/2011.13127)). At build time you compile the operations your language needs into an object file and extract the unrelocated machine code plus its relocation records. That is a stencil. At runtime, code generation is a memcpy followed by a few scalar additions to patch in constants and addresses. No instruction selection, no register allocator, no assembler.

The reported results: code generated two orders of magnitude faster than LLVM at `-O0` and three orders faster than higher optimization levels, running an order of magnitude faster than interpretation and 14% faster than LLVM `-O0` output, with their WebAssembly compiler generating code 4.9x to 6.5x faster than Liftoff, Chrome's Wasm baseline compiler.

It has been used in production since. A [January 2026 writeup from Cognica](https://www.cognica.io/en/blog/posts/2026-01-17-copy-and-patch-jit) reports copy and patch being 50 to 200x faster than LLVM at compilation in a SQL engine, making JIT worthwhile for queries that run in single digit milliseconds, and describes an optimizing tier layered on top that builds an IR over the stencils for another 2x. CPython ships an experimental copy and patch JIT from 3.13.

The caveat that matters most to us comes from Haoran Xu's own [writeup](https://sillycross.github.io/2023/05/12/2023-05-12/): vanilla copy and patch still needs a lot of manual work to write the stencils and runtime logic, and naive stencils produce poor code because every operation becomes a call to another function unless you restructure the handlers into continuation passing style. That restructuring is the actual work, and document 06 plans for it.

### 1.2 Deegen, and generating the interpreter and the baseline JIT together

Xu and Kjolstad, "Deegen: A JIT-Capable VM Generator for Dynamic Languages", PACMPL / OOPSLA 2025 ([DOI](https://dl.acm.org/doi/10.1145/3798246), [arXiv 2411.11469](https://arxiv.org/pdf/2411.11469)). You describe each bytecode's semantics as a C++ function. Deegen generates an interpreter, a baseline JIT, and the tier switching logic between them, applying bytecode specialization and quickening, register pinning, tag register optimization, call inline caching, generic inline caching, polymorphic ICs, IC inline slabs, type check removal, slow path outlining, hot and cold code splitting, and OSR entry, all automatically.

The numbers, across 44 benchmarks: the generated Lua interpreter is 2.79x faster than PUC Lua's interpreter and 1.31x faster than LuaJIT's hand written assembly interpreter. The generated baseline JIT has negligible compilation cost and runs 4.60x faster than PUC Lua, landing 33% slower than LuaJIT's full optimizing JIT on average while being faster on 13 of the 44 benchmarks.

An independent line of work reaches the same conclusion from another direction: Palumbo et al., "Meta-compilation of Baseline JIT Compilers with Druid", Programming Journal 2025 ([arXiv 2502.20543](https://arxiv.org/abs/2502.20543), [code](https://github.com/Alamvic/druid)) generates a baseline JIT frontend from an annotated interpreter for Pharo, producing 13.4k lines of generated compiler equivalent to a hand written 3.3k, needing changes at only 60 call sites in the interpreter. The follow up, "Are Abstract-interpreter Baseline JITs Worth it?" at CGO 2026, reports 12% smaller machine code and 10% faster execution from adding abstract interpretation to the generated compiler.

**What this means for katsu.** The generated tier approach is not speculative any more, it has two independent implementations and published numbers. Document 05 and document 06 build on it directly, which is why our bytecode semantics live in a macro DSL rather than in hand written match arms. The open risk is that both existing results are for languages simpler than JavaScript, and document 17 makes that open question one.

### 1.3 Garbage collectors you can depend on instead of writing

[MMTk](https://www.mmtk.io/status) is the Rust framework: allocators, spaces and work packets that compose into GC plans, with three officially supported bindings (JikesRVM, OpenJDK, Ruby) and third party ones for GHC, PyPy and Scala Native. Ruby 3.4 shipped modular GC with MMTk as an option, although its default plan there is MarkSweep and Immix support was still in progress at the time of that writeup.

Important caveat found while checking: LXR, the reference counting plus Immix collector from Zhao, Blackburn and McKinley (PLDI 2022) that reports better throughput and pause times than production collectors, is **not merged into mmtk-core**. It lives on [a branch of a fork](https://github.com/wenyuzhao/mmtk-core/tree/lxr). So LXR is a research option, not a dependency.

The alternative is [Whippet](https://github.com/wingo/whippet), Andy Wingo's collector library, described in "Nofl: A Precise Immix" ([arXiv 2503.16971](https://arxiv.org/abs/2503.16971), March 2025). Nofl pushes Immix's reclamation granularity down to the allocator's minimum alignment, fixing Immix's worst case where one tiny object pins two 128 byte lines. Whippet is a no dependency library meant to be embedded in the host runtime's source tree, offering semi space, parallel copying, the Nofl based mostly marking collector, and a Boehm shim, with the concrete collector chosen at compile time and enough API surface for the embedder to open code allocation fast paths, safepoints and write barriers. It reports outperforming copying and mark sweep collectors at tight to adequate heap sizes.

**What this means for katsu.** Document 08 specifies the GC interface first and the collector second, evaluates MMTk and Whippet against our own workloads at M4, and commits then. Whippet is C, so using it means an FFI layer; MMTk is Rust, so it is the default unless measurements say otherwise. Both are enormously cheaper than writing our own.

## 2. Facts about the engines we are competing with

### 2.1 V8 left sea of nodes

V8's [March 2025 post](https://v8.dev/blog/leaving-the-sea-of-nodes) documents a three year migration off sea of nodes to Turboshaft, a conventional CFG IR. The whole JavaScript backend and all of WebAssembly now use Turboshaft, with compilation time roughly halved and code quality equal or better. The remaining sea of nodes users are the builtin pipeline and the JavaScript frontend, both being replaced, the latter by "Turbolev", which feeds Maglev's CFG IR straight into Turboshaft.

Maglev itself, V8's mid tier, [deliberately chose](https://v8.dev/blog/maglev) traditional SSA over a CFG rather than TurboFan's "more flexible but cache unfriendly sea-of-nodes representation".

The tier up thresholds, from Intel's profile guided tiering work: Ignition to Sparkplug at 8 invocations, Sparkplug to Maglev at 500 with the counter resetting to zero if feedback changes, Maglev to TurboFan at 6000.

**What this means for katsu.** No sea of nodes. And those thresholds are a free gift, they are the result of years of tuning at Google and they are a much better starting point than numbers we invent.

### 2.2 Pointer compression is where the memory is

V8's [pointer compression post](https://v8.dev/blog/pointer-compression) reports up to 43% heap size reduction and up to 20% reduction in Chrome's renderer memory, because tagged values are around 70% of the heap on real sites. Electron cites up to 40% heap reduction plus 5 to 10% CPU and GC improvement. Going the other way, a V8 developer put the cost of disabling it at 60 to 70% more memory.

The price is a 4 GB cap per heap, and the [V8 sandbox](https://v8.dev/blog/sandbox) is built on top of it: a terabyte of virtual address space containing the heap cage, with references as offsets from a base, native pointers banned from the cage and reached through external pointer tables instead. Cloudflare's [hardening writeup](https://blog.cloudflare.com/safe-in-the-sandbox-security-hardening-for-cloudflare-workers/) describes 4 GiB of cage plus 4 GiB for buffers with 32 GiB unmapped guard regions around it. V8 added multi cage mode with isolate groups for multi tenant embedders.

One consequence worth writing down: because compressed pointers have no spare bits, the entire heap has to carry one memory tag, which makes ARM MTE useless for detecting corruption between objects inside the heap.

**What this means for katsu.** We do pointer compression, we build the cage, and document 07 carries the 4 GB cap as an explicit product limitation with `--no-pointer-compression` as the escape hatch. It also means we cannot claim a 10x heap win over Node, because Node already has this. Document 02.

### 2.3 The interpreter versus JIT gap is roughly 10x

The comparison numbers found while checking: V8 in jitless mode is about 3x faster than QuickJS, and V8 with the JIT about 30x faster. An independent macOS run on one compute benchmark: V8 410ms, JavaScriptCore 271ms, QuickJS 6101ms, Hermes bytecode 2085ms.

**What this means for katsu.** This is the single most sobering number in the document and it sets the shape of document 02. A no JIT runtime is 10x slower than Node on compute, not 10x faster. Everything in the 10x speed claim has to come from tiers that actually generate machine code, from AOT with types, or from axes that are not peak compute at all.

### 2.4 Static Hermes: the AOT idea works, with a condition

Static Hermes compiles soundly typed JavaScript ahead of time to native code via LLVM, with `shermes -emit-c` producing a C program. Tzvetan Mikov's headline result was a [300x speedup on a microbenchmark](https://news.ycombinator.com/item?id=37459829), and the mechanism is sound types: the types the developer writes are the types the engine uses, so dynamic checks and deopt disappear.

The counter example matters as much. A [ClojureScript based benchmark](https://romanliutikov.com/blog/native-apps-with-clojurescript-react-and-static-hermes) found an optimized Static Hermes executable running around 6450ms against roughly 1100ms for the same code on Node and Bun, about 6x slower, with the author concluding that generated native code cannot compete with a modern JIT on idiomatic dynamic JavaScript. The compensation was binary size, a few MB against roughly 60MB for embedding a full VM.

**What this means for katsu.** AOT is 300x on typed hot loops and 6x slower on untyped idiomatic code, and both facts are true at once. Document 09 is built entirely around that gap: types make AOT win, absence of types makes AOT lose to a JIT, so AOT mode needs profile guided speculation and an embedded interpreter, not just static compilation. It is also why AOT is not the default mode.

### 2.5 The Rust engines that exist

[Boa](https://github.com/boa-dev/boa) passes 95.5% of test262, roughly 51k of about 53k tests, which puts it fourth overall and above JavaScriptCore on [test262.fyi](https://test262.fyi). Its Temporal implementation reached 100% on the Temporal suite and the extracted `temporal_rs` crate is now used by V8 and Node. Boa is interpreter only and its conformance growth has slowed deliberately as the team shifted focus to performance.

[Nova](https://github.com/trynova/nova) is the interesting one architecturally. It is data oriented: every heap value lives in a type specific vector, every heap reference is a type discriminated 32 bit index rather than a pointer, and objects are split aggressively across parallel vectors so that reading one field does not drag unused fields into cache. The [Web Engines Hackfest 2024 slides](https://webengineshackfest.org/2024/slides/nova_javascript_engine_exploring_a_data-oriented_engine_design_by_aapo_alasuutari.pdf) make the case: indexes are automatic pointer compression, they let one value index several vectors, and reinterpreting an index as the wrong type changes which arena you read from rather than producing a type confusion. The cost is extra indirection and a GC that must compact vectors to keep them dense. Nova is still explicitly not ready for use.

**What this means for katsu.** Boa is the best available reference for correct spec implementation in Rust and we should read its builtins constantly. Nova's index based design is a genuine alternative to our tagged pointer plus cage plan, it gets pointer compression for free, and document 07 records why we are not taking it: inline caches and JIT generated code want a direct memory address to load from, and an extra arena indirection on every property access is a cost the JIT cannot optimize away.

## 3. Rust ecosystem: what we take and what it costs

### 3.1 The parser

[Oxc](https://oxc.rs/) publishes a parser benchmark of 26.3ms against SWC's 84.1ms and Biome's 130.1ms on typescript.js, claims 3x faster than SWC, and passes all test262 stage 4 tests. It is part of VoidZero's toolchain and powers Rolldown, which is Vite 8's bundler. `oxc-resolver` is MIT licensed and was last committed on 26 August 2026, so it is actively maintained, and it claims 28x faster than enhanced-resolve.

**What this means for katsu.** We use oxc for parsing and evaluate `oxc-resolver` for module resolution rather than writing either. A conformant JavaScript parser is around 30k lines of tedium with zero differentiating value. The cost is an AST we do not control, and the mitigation is that exactly one file consumes it. SWC is the fallback if governance or licensing changes.

### 3.2 Regular expressions

The `regex` crate cannot implement JavaScript's `RegExp` because it deliberately excludes backreferences and lookaround. [`regress`](https://github.com/ridiculousfish/regress) is a backtracking engine targeting ECMAScript syntax, supports backreferences, variable width lookbehind with capture groups and the `v` flag, and crucially offers UTF-16 and UCS-2 input modes where surrogate pairs split freely, which is exactly what strict JavaScript semantics require. Boa uses it. It has over 32 million downloads. In BurntSushi's rebar benchmarks it reaches around 289 MB/s on the wild unstructured-to-json workloads, beating non-JIT PCRE2 and Python's `re`.

**What this means for katsu.** Use `regress`, add a step limit and a timeout because a backtracker with untrusted patterns is a denial of service waiting to happen, and revisit only if regex turns out to dominate a real benchmark.

### 3.3 Guaranteed tail calls are not available

The `become` keyword is nightly only behind `feature(explicit_tail_calls)`, documented as incomplete. LLVM backend codegen landed in 2025, other backends stub out and ICE. The [Trifecta Tech project goal](https://trifectatech.org/blog/tail-calls-project-goal/) plans most of the work in 2026 and explicitly targets stabilization in **2027**, and it needs funding. Known unfixed edge cases include RPIT plus tail calls. There is related experimental work on `#[loop_match]` for computed goto style dispatch and an `extern "tail"` convention over LLVM's `tailcc`.

There is at least one worked example of a tail call interpreter in nightly Rust ([Matt Keeter, April 2026](https://www.mattkeeter.com/blog/2026-04-05-tailcall/)).

**What this means for katsu.** Document 05 cannot assume tail call dispatch on stable Rust before 2027. The interpreter dispatch strategy needs a plan that works on stable today and gets faster when `become` lands, and `#[loop_match]` is worth tracking as the nearer term option. This is open question three.

### 3.4 Deoptimization support in Cranelift

Cranelift has [user stack maps](https://bytecodealliance.org/articles/new-stack-maps-for-wasmtime): the CLIF producer is responsible for identifying live GC values, spilling them, and attaching stack map entries at safepoints, via `declare_needs_stack_map` and `append_user_stack_map_entry`. Safepoint spills and reloads are ordinary loads and stores in the IR, so alias analysis can see them. That is the right foundation for deopt, since deopt also needs precise live state at specific points.

What I could not find is any Cranelift support for deoptimization proper: tiering down, deopt points, or frame state reconstruction. **[unverified, and the absence of results is itself informative]**

**What this means for katsu.** Document 06 treats the tier 2 backend as an open decision with two candidates: Cranelift with deopt built on user stack maps, or our own backend emitting machine code directly. Document 17 makes it open question two, and M6 is where it gets decided by prototype rather than argument.

### 3.5 napi-rs is the interop bar to clear

[napi-rs](https://napi.rs/) is what the ecosystem already uses for Rust in Node: swc, Rolldown, lightningcss, Rspack and others ship through it. It generates TypeScript definitions, cross compiles, and ships prebuilt per target binaries with a WASI fallback. It maps `Result` to a thrown exception and `Option` to `undefined | T`. With the `tokio_rt` feature it runs a tokio runtime on an extra thread and returns promises.

Its two documented friction points are exactly the ones our design has to beat: overhead when passing large or complex values across the boundary because types must be converted on each crossing, and the CI cost of shipping binaries for Windows, macOS x64 and arm64, and Linux glibc and musl.

**What this means for katsu.** Document 11 is measured against napi-rs, not against nothing. Our advantage is structural: we own both sides, so a Rust struct can be exposed as a JavaScript object with a real shape and no per call conversion, and there is no prebuilt binary problem because the runtime is the binary.

## 4. The Node.js target as of today

Node 26 is the Current line and **Node 24 is the Active LTS**. Node 26.7.0 released 5 August 2026; Node 26 went out 5 May 2026 with Temporal enabled by default, V8 14.6 from Chromium 146, an experimental FFI module and Undici 8. Node 25 hit end of life on 1 June 2026. Node 26 becomes LTS in October 2026, and it is the last release under the current model: from Node 27 the cycle becomes annual with every major going LTS after six months as Current.

Baseline memory, from Node's own docs and tooling samples: `process.memoryUsage()` showing around 25.8 MB RSS with 5.2 MB heapTotal in one example and around 42.4 MB RSS in another. The Node binary is roughly 110 to 160 MB on disk against under 1 MB for QuickJS.

AWS's LLRT claims up to 10x faster startup and up to 2x lower cost on Lambda with a runtime under 2 MB, but a large part of that comes from a constraint we cannot adopt: it requires bundling everything into a single JavaScript file, eliminating Node's module resolution filesystem probing entirely, and it embeds the AWS SDK in the binary. The much quoted "Node 1.5s versus LLRT under 100ms" benchmark is one that imports the AWS SDK, which LLRT has compiled in.

**What this means for katsu.** Compatibility target is Node 24 LTS behaviour with Node 26 features tracked. The 25 to 45 MB RSS baseline is the number the 10x memory goal is measured against, so the target is 2.5 to 4.5 MB. And the LLRT lesson is that a big part of Node's startup cost is module resolution doing filesystem syscalls, which is a cost we can attack honestly with caching rather than by requiring a bundler.

## 5. The Node-API question

Node-API is ABI stable across Node versions within a version level, which is the whole reason it exists. Deno reports `process.versions.napi` as 10. Bun implements it. The ecosystem's modern native packages ship through napi-rs or node-gyp with N-API.

The holdouts are real. Bun's [engineering post](https://bun.com/blog/how-bun-supports-v8-apis-without-using-v8-part-1) says supporting raw V8 APIs was the 11th highest open issue on their tracker by reaction count, and describes how their first approach, reinterpreting `JSValue` bits as `Local<T>` on the assumption both are 8 bytes, broke as soon as they hit internal fields, because V8's `GetInternalField` is inlined into the addon and does pointer arithmetic against a memory layout JavaScriptCore does not have. Reported breakage in the wild: bcrypt does not work on Bun, canvas is broken with no clean workaround.

I could not find a reliable percentage split of Node-API versus raw V8 API usage across npm. **[unverified]** That number is load bearing for document 10's scope and document 17 says to measure it directly by scanning the registry rather than assuming it.

## 6. Standards scope

WinterCG became **Ecma TC55 (WinterTC)** in 2025, and the Minimum Common API is now **ECMA-429**, with the draft and published editions in [proposal-minimum-common-api](https://github.com/WinterTC55/proposal-minimum-common-api). Active workstreams as of TPAC 2025 include the Minimum Common Web API, a WPT subset test suite, a unified Sockets API for TCP and TLS, and a CLI API for environment variables, arguments and cwd. There is upstream work to add server side conformance modes to Fetch, since server runtimes have no origin and all of them knowingly violate the cross origin parts of the spec.

**What this means for katsu.** ECMA-429 plus its WPT subset is our web API scope, and it is a much better scoping tool than guessing which browser APIs a server needs. It also gives us a conformance number to publish alongside test262.

## 7. Security posture

The threat model that produced the V8 sandbox is the one that applies to us: an attacker uses a JIT type confusion bug to corrupt something inside the heap, such as an ArrayBuffer's backing pointer, and turns that into read and write over the whole process. Rust removes the accidental version of this and does nothing about the deliberate version, because a logic bug in the compiler that eliminates a bounds check produces memory unsafety regardless of what the compiler is written in.

On Apple Silicon the mechanics are specific. MAP_JIT pages enforce W^X per thread. You call `pthread_jit_write_protect_np(false)` to write and `(true)` to execute, permissions are per thread, and Apple warns against giving one thread write access while another has execute access on the same region. Use the pthread call rather than `mprotect`: CPython measured a 1.4% overall speedup from that change alone. Newer code should prefer `pthread_jit_write_with_callback_np`, which only allows callbacks registered through `PTHREAD_JIT_WRITE_ALLOW_CALLBACKS_NP`, because attacks exist that induce unintended calls to the plain toggle via `dlsym`. The classic bug is leaking write mode on an early return, which is why RAII guards are mandatory and not optional. You also need the JIT entitlement under the Hardened Runtime, and `sys_icache_invalidate` after writing.

**What this means for katsu.** Document 14 makes the W^X guard an RAII type that cannot be leaked, makes the heap cage a requirement rather than a hardening step, and puts differential fuzzing of the JIT against the interpreter in CI from M2 rather than after 1.0.

## 8. Adjacent research worth tracking

"CoSSJIT: Combining Static Analysis and Speculation in JIT Compilers", OOPSLA 2025 ([DOI](https://dl.acm.org/doi/10.1145/3763149)), enriches static analysis with speculative optimization, applied to aggressive stack allocation on a production JVM. Directly relevant to document 09, where we want static analysis of TypeScript types combined with runtime speculation.

"Formally verified speculation and deoptimization in a JIT compiler", POPL 2021 ([paper](https://janvitek.org/pubs/popl21.pdf)), and "Correctness of speculative optimizations with dynamic deoptimization", POPL 2018. These give a model IR where deopt sync points and speculation assumptions are explicit in the IR, which is exactly the discipline document 06 needs so that our optimizer cannot silently invalidate an assumption.

"Understanding and Finding JIT Compiler Performance Bugs", OOPSLA 2026. Worth reading before we build the benchmark harness in document 15.

ISMM 2026 is co-located with PLDI 2026 in Boulder, 17 to 19 June, which is where the next round of GC results will land.

## 9. What could not be verified

Listed here so nobody treats them as established.

- Whether Cranelift can express deoptimization cleanly in 2026.
- The real Node-API versus V8 API split across npm packages with native code.
- Nova's current state and whether the index based design holds up under a JIT.
- Whether copy and patch stencils work well for polymorphic property access, which nobody has published.
- Whether MMTk's binding overhead is acceptable for a language with JavaScript's allocation rate.
- The exact current status of Node's `--experimental-strip-types` restrictions in Node 24 and 26.
- Whether ECMA-429 has a published first edition or is still draft.
