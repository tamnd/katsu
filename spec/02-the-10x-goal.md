# The 10x goal, broken into axes

The mission is 10x faster than Node.js and 10x less memory. This document is the one that decides whether that claim is honest, because "10x faster" is not a single number and the axes behave completely differently. On four of them 10x is reachable and other projects have already demonstrated something close. On two of them it is not reachable by anyone, and pretending otherwise would make everything else we publish untrustworthy.

Every number Node is compared against here comes from document 01, which has the sources.

## 2.1 The six axes

| # | Axis | Node today | katsu target | 10x? | Confidence |
|---|---|---|---|---|---|
| 1 | Cold start, hello world | 25 to 40 ms | under 3 ms JIT, under 1 ms AOT | yes, 10x to 40x | high |
| 2 | Baseline RSS at idle | 25 to 45 MB | under 4 MB JIT, under 3 MB AOT | yes, 10x | high |
| 3 | Distribution size | 110 to 160 MB | under 15 MB | yes, 10x | high |
| 4 | Typed compute, AOT | 1x baseline | 10x to 100x on typed hot code | yes, on typed code only | medium |
| 5 | Server request throughput | 1x baseline | 2x to 4x | no, and we will not claim it | medium |
| 6 | Peak untyped compute, JIT | 1x baseline | 0.5x at M6, 1x at 1.0, 1.5x later | no, never | high that 10x is impossible |
| 7 | Heap under load, live objects | 1x baseline | 1.5x to 2x less | no | high |

Seven rows for six axes because distribution size is a memory axis in every sense that matters to a container image or a Lambda layer, and it is the easiest 10x on the list.

The headline claim we are allowed to make is therefore: **10x faster to start, 10x smaller in memory and on disk, 10x to 100x faster on typed compute in AOT mode, competitive with Node on everything else.** That is a strong claim, it is defensible with measurements, and it does not fall apart the first time somebody runs an Octane benchmark.

## 2.2 Axis 1: cold start

Node spends its startup on process init, V8 snapshot deserialization, bootstrapping the Node internals, and then module resolution, which is filesystem syscalls all the way down. AWS's LLRT reports up to 10x faster start than Node on Lambda, and a big part of that comes from a rule we are not willing to adopt: it requires the user to bundle everything into one JavaScript file so that module resolution disappears entirely.

What we do instead:

**A realm snapshot.** The global object, the prototype chain, every builtin, and the interned atom table are constructed once at build time and serialized into a memory mapped image in the binary. Startup maps it and fixes up a small relocation table instead of executing a bootstrap. This is V8's snapshot idea and it is the single biggest startup lever available.

**A resolution cache.** Node's resolution algorithm stats its way up the directory tree for every import. We cache the resolved graph, keyed by the content hash of every `package.json` that participated plus the directory mtimes, so a second run of the same program does approximately zero resolution syscalls. This gets us most of LLRT's win without forcing anyone to bundle.

**A bytecode cache** so the second run of a program skips parsing and lowering, keyed by content hash and bytecode version, never by mtime.

**Lazy everything else.** No module is initialized until required, no function is lowered until called, no inline cache is allocated until its site executes.

In AOT mode all four of these collapse: there is no parse, no lowering, no resolution and no snapshot deserialization, only process start and module init. Under 1 ms is a realistic target and it is what makes katsu interesting for serverless.

**What this forces:** the snapshot format is a design constraint on the object model from day one, because you cannot snapshot a heap full of absolute pointers. This is one of several reasons document 07 uses a compressed cage. Also, laziness has to be pervasive rather than a later optimization, which is why document 04 specifies lazy lowering rather than treating it as tuning.

## 2.3 Axis 2 and 3: memory and size

Node's 25 to 45 MB idle RSS is not V8 being wasteful, it is the sum of a large binary being paged in, a snapshot being deserialized into a heap, and a large JavaScript implemented standard library being instantiated. Our budget, as a target rather than a measurement:

| Component | Budget |
|---|---|
| Binary pages actually touched at idle | 1.5 MB |
| Realm snapshot, mapped copy on write, only dirtied pages counted | 400 KB |
| Interned atoms and shape table | 200 KB |
| Initial GC heap, before any user allocation | 512 KB |
| Interpreter state, stacks, per isolate structures | 256 KB |
| Event loop, io_uring rings and registered buffers | 256 KB |
| Node compat layer, only the modules actually imported | 300 KB |
| Headroom | 600 KB |
| **Total at idle** | **under 4 MB** |

Every line in that table is a thing somebody has to enforce, so document 08 turns it into a test that fails CI when it regresses. A memory budget that is not a test is a wish.

Distribution size falls out of the same discipline. Node ships 110 to 160 MB. QuickJS ships under 1 MB with no JIT. We have a JIT, a Node compatibility layer and ICU, so under 15 MB is the target, with ICU as the largest single item and `Intl` data loadable separately for size sensitive deployments.

**What this forces:** three unpleasant but necessary policies. Feedback vectors and inline caches are allocated on first execution of the site, not on function entry. Compiled code is evictable, with an LRU and a code cache budget, so a long running process that touched 50,000 functions once does not carry 50,000 compiled bodies forever. And the Node layer is lazily instantiated per module, so importing `node:path` does not pay for `node:crypto`.

It also forces an explicit tie break rule, written here so it does not get relitigated in every code review: **when a technique buys speed by spending memory, and the memory budget is at its limit, the budget wins.** The exception is anything on axis 6, where we are already behind and cannot afford to give up more.

## 2.4 Axis 4: typed compute in AOT mode, where the real 10x lives

Static Hermes reported a 300x speedup on a microbenchmark by requiring sound types so that the compiler can drop dynamic checks entirely. The same technique, measured on idiomatic ClojureScript output, ran about 6x slower than Node.

Both results are true and they define the shape of our AOT mode. Typed numeric code with monomorphic call sites compiles to something close to what a Rust programmer would have written, and beats a JIT because there is no boxing, no type check and no deopt guard on the hot path. Untyped, polymorphic, allocation heavy code compiled statically loses badly to a JIT, because the JIT has runtime type feedback and the static compiler has nothing.

So AOT mode is not "the fast mode". It is the mode that is enormously faster on the parts of your program you have typed and instrumented, and roughly interpreter speed on the parts you have not, unless you give it a profile. That asymmetry is why document 09 specifies profile guided AOT: you run `katsu run --profile`, the profile records the types actually observed at each site, and the AOT compiler speculates on them with guards that deopt into the embedded interpreter. That turns AOT from a static compiler into an offline JIT with a persistent profile, which is the only version of this idea that wins on real programs.

**What this forces:** the embedded interpreter in every AOT binary, the profile format as a stable serialized artifact, and honesty in the documentation that `katsu build` without a profile and without types is not going to be faster than `katsu run`.

## 2.5 Axis 5: server throughput

For an HTTP service, the engine is usually not the bottleneck. The syscall path, the TLS layer, the HTTP parser, the stream machinery and the allocator are. Node's overhead here is real: streams are implemented in JavaScript, `Buffer` allocations churn, and libuv's threadpool handles file I/O.

The evidence on io_uring is mixed and worth taking seriously rather than repeating vendor claims. Recent benchmarks find TCP echo performance nearly identical between io_uring and epoll, because syscalls are not the dominant cost in the packet lifecycle, and there are workloads where epoll wins outright. What does scale is the thread per core model: Monoio reports roughly parity with Tokio on one core, about 2x at four cores and approaching 3x at sixteen, and the authors attribute much of that to thread per core rather than to io_uring itself.

So our target is 2x to 4x on requests per second, coming from thread per core isolates with no cross thread work stealing, streams implemented in Rust rather than JavaScript, an HTTP parser that writes into reusable buffers, and zero copy from the socket into a JavaScript `Buffer` view. Not 10x. We will publish the number we measure.

**What this forces:** thread per core is an architecture decision, not a tuning flag, and it has to be made before the event loop is written. It also means our async model has to avoid requiring `Send` on task state, which is document 12.

## 2.6 Axis 6: peak untyped compute, where 10x is impossible

This is the axis where a project like this normally lies, so here is the arithmetic.

V8 with the JIT is roughly 30x faster than QuickJS on compute, and V8 in jitless mode is roughly 3x faster than QuickJS. On one independent compute benchmark, V8 ran 410ms, JavaScriptCore 271ms, Hermes bytecode 2085ms and QuickJS 6101ms.

TurboFan is a mature optimizing compiler with type feedback, escape analysis, inlining and twenty years of tuning, generating code for the same language semantics we have. There is no technique that makes the same semantics run 10x faster than that. If there were, Google would have shipped it. The only way to beat V8 by 10x on compute is to change the semantics, which is exactly what AOT with sound types does, and that is axis 4, not this one.

Our honest trajectory on this axis:

At M5, with only the interpreter and the copy and patch baseline, we expect to be somewhere between QuickJS and V8's jitless mode, which is to say several times slower than Node. That is not a failure, it is the expected shape, and it is the same place LLRT sits today while being a useful product.

At M6, with the optimizing tier, roughly 0.5x of V8 on compute benchmarks is a good result. Deegen's generated baseline JIT landed 33% slower than LuaJIT's full optimizing JIT, which is encouraging for a much simpler language.

At 1.0 the target is parity within noise on most of the benchmark suite, with known losses on the benchmarks that reward the specific optimizations we have not written yet.

Long term, beating V8 on this axis at all is a real achievement and 1.5x would be a headline. Ten is not on the table and saying so in the README is what buys credibility for the four axes where our numbers are real.

**What this forces:** the benchmark harness in document 15 reports this axis prominently rather than burying it, and the README has a table with all seven rows, not a single number.

## 2.7 Axis 7: heap under load

If a program holds ten million live objects, the heap is dominated by those objects and their property storage. V8 already does pointer compression, so its tagged slots are already 4 bytes. We are not going to find 10x there.

Where we can win 1.5x to 2x: smaller object headers, no per function feedback vector for functions that never get hot, no compiled code for cold functions, shape trees shared aggressively across objects with identical layout history, out of line property storage sized exactly rather than doubled, and a collector with better fragmentation behaviour than V8's, which is what Nofl and Immix are for.

Where we can lose: an immature collector fragments, and our first collector will be worse than V8's under adversarial allocation patterns. Document 08 is explicit that this axis is the one most likely to embarrass us at M4 and that fixing it is a measurement loop, not a design decision.

## 2.8 What we say in public

The rules, so that every benchmark post and README revision follows the same discipline:

Always name the workload. "10x faster" without a workload is marketing. "10x faster cold start on hello world, 3.2x on an Express app with 40 dependencies" is a fact.

Always publish the axis where we lose. If a benchmark chart has six bars and we win five, publish six bars.

Never compare a warmed katsu against a cold Node, never compare AOT katsu against interpreted Node without labelling it, and never quote a microbenchmark speedup as a program speedup. The Static Hermes 300x number is the cautionary example: it is real, it is correctly measured, and it made people expect something that idiomatic code did not deliver.

Report memory as RSS measured the same way for both runtimes on the same machine, with the measurement script in the repo.

Document 15 turns these rules into the harness.

## 2.9 The tension nobody escapes

Speed costs memory. Inline caches, feedback vectors, compiled code, shape tables and profile data are all memory spent to buy time. A runtime that is 10x smaller than Node has strictly less of all of it.

We resolve this by tiering the memory the same way we tier the code. Cold functions carry bytecode and nothing else. Warm functions get a feedback vector and inline caches. Hot functions get compiled code, and it is evictable. A process that runs one hot loop pays for one hot loop.

That is the design that makes both goals survivable at once, and it is the reason the tier thresholds in document 06 are as much a memory policy as a performance policy.
