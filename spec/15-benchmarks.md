# Benchmarks

## 15.1 The rules come before the numbers

Document 02.8 sets the reporting rules and they are repeated here because this is the document where they get broken if anyone is going to break them.

Every published number names the machine, the operating system, the exact versions of everything compared, the workload source, and the number of runs. Every number is reproducible by a third party from a command in our repository. Losses are published alongside wins, per axis, with the same prominence. We never publish a geometric mean across axes that mixes a 40x startup win with a 0.6x compute loss, because that number is designed to mislead and everyone in this field knows it.

And the claim is always per axis. "10x faster than Node" with no qualifier is a lie regardless of what any single benchmark says.

## 15.2 The compute suite

**JetStream 3.0** is the primary compute benchmark. It was released 31 March 2026 under the same open governance model as Speedometer, built over about eighteen months from more than 200 pull requests across more than 70 workloads. It absorbed its predecessors rather than replacing them, so it contains parts of SunSpider, Octane 2, JetStream 2, ARES-6, the Web Tooling Benchmark and Kraken-inspired workloads, and adds new ones for WebAssembly, Web Workers, promises, async iteration, unicode regular expressions and JavaScript parsing. Most workloads weigh startup, worst case and average case equally, which is a scoring choice that suits us: a suite that only measured steady state peak would flatter V8 and hide the axis we win on.

It is also designed to run in engine shells like d8 rather than a browser, which means it runs in our shell with no DOM, and it is the reason we do not need to build one.

For reference, Chrome reported 469 on JetStream 3 and 61 on Speedometer 3.1 on a MacBook Pro M5 in 2026. Those are the numbers a serious engine is measured against.

**Speedometer 3.1 is not applicable.** It measures web application responsiveness through DOM interactions and we have no DOM. Saying so plainly is better than quoting a number from a suite that does not apply.

**Microbenchmarks** exist as a separate, clearly labeled set: property access at each cache state, arithmetic on each type, string building, array iteration, closure creation, exception throw and catch, generator resume, promise resolution, `Map` and `Set` operations, and JSON round trips. They are diagnostic tools for finding where time goes, they are never published as headline numbers, and the label says so.

## 15.3 The axes from document 02, and how each is measured

**Cold start.** Wall clock from process spawn to first line of user code executed, and to process exit, for: an empty file, a hello world, a file importing twenty typical dependencies, and a 5 MB bundle. Measured with `hyperfine` over a hundred runs, medians and full distributions reported. Both JIT and AOT modes, against Node, Deno, Bun and LLRT.

Note a data quality problem worth being explicit about. Published comparisons vary widely: one 2026 source reports cold starts of 8 to 15ms for Bun, 40 to 60ms for Deno and 60 to 120ms for Node, while our own figure for Node in document 02.2 is 25 to 40ms. Those disagree because they are measuring different things on different hardware with different amounts of application code. **This is exactly why our harness ships and why the machine is named.** We publish our own measurements taken the same way for every runtime, and we do not cite anyone else's numbers as though they were ours.

**Baseline memory.** RSS at idle after startup, RSS after an empty HTTP server binds, and RSS for the twenty dependency case. Measured from outside the process, since a runtime's self reported heap size is not what a container's memory limit counts. The comparison points from 2026 blog measurements are roughly 18 MB for Bun, 30 MB for Deno and 40 MB for Node at idle, and document 02.3 targets under 4 MB.

**Distribution size.** The size of the installed artifact and of the smallest container image that can run a hello world, for every runtime.

**Typed compute in AOT mode.** Numeric kernels with real types: n-body, matrix multiply, FFT, ray tracing, JSON parsing, a tokenizer, image resizing. Compared against Node, against ourselves in JIT mode, and against the same algorithm written in Rust, because the Rust number is the ceiling and showing it is more honest than not.

**Server throughput.** `wrk` and `oha` against plain HTTP, a JSON echo, static file serving, and Express and Fastify applications. Latency percentiles including p99 and p99.9, not just requests per second, since tail latency is where a garbage collector's real cost shows up. Also measured at fixed concurrency levels with memory tracked simultaneously, since throughput at unbounded memory is not a useful result.

**Peak compute in JIT mode.** JetStream 3, reported as the suite reports it, wins and losses both.

**Heap under load.** A workload that allocates persistently, measuring peak RSS and steady state RSS to hold the same live set, plus fragmentation over hours from the soak test in document 14.10.

## 15.4 Who we compare against

Node LTS, which is the reference and the thing the claim is about. Bun, which is the strongest competitor on startup and memory. Deno, for the Node compatibility comparison. LLRT, because it is the extreme startup and size point and it is honest to show a case where somebody beats us. QuickJS, as the interpreter baseline. Perry and Static Hermes for AOT mode specifically, since they are the closest comparable work. And handwritten Rust as the ceiling on typed compute.

Every runtime is at its latest stable release, configured the way its own documentation recommends, with no flags we would not recommend to a user. Benchmarking a competitor in a bad configuration is the fastest way to lose the credibility this whole document exists to build.

## 15.5 The harness

One repository, one command, results as JSON.

Fixed hardware for published numbers: a named x86-64 machine and a named aarch64 machine, with CPU frequency scaling pinned, turbo disabled, processes pinned to cores, and nothing else running. Cloud instances are too noisy for anything but relative trends and are labeled as such when used.

The reference machines, so that a number in this repository can be attributed to one of them by name:

| Name | Machine | Notes |
|---|---|---|
| `gamingpc` | Intel Core i9-13900K, 32 threads, 32 GB, Ubuntu under WSL2 on Windows 11 | The x86-64 Linux reference. Benchmarks are pinned to a single performance core with `taskset`. It is a desktop that is otherwise idle, and the WSL2 layer is a virtual machine, both of which are stated next to any number taken here. |
| `gamingpc-win` | The same box, running Windows 11 natively rather than through WSL2 | The Windows reference. Benchmarks are pinned with `start /affinity`, which matters more here than anywhere else because a 13900K has performance and efficiency cores and an unpinned run lands on either. An unpinned run measured roughly half the throughput of a pinned one on every microbenchmark, which is the efficiency core and not the code. |
| `m4` | Apple M4, 10 cores, 24 GB, macOS 15 | The aarch64 reference. A laptop, so thermal behaviour over a long suite is a real effect and long running benchmarks say so. |

Two of those are the same physical machine, which is deliberate. Running the same commit under Windows and under Linux on identical hardware isolates the operating system from everything else, and the first time it was done it found a quadratic commit pattern in the heap that Linux had been hiding. See `spec/07-object-model.md` 7.2.2.

There are three cloud boxes on hand, `server1`, `server2` and `server3`, and none of them are reference machines. They are shared AMD EPYC instances carrying load averages between eight and fourteen while doing other work, which makes them worse than either laptop for a comparison and fine for nothing except checking that something builds and runs on a machine we do not control. A number from one of them is labelled as such or it does not get published.

Turbo pinning and frequency scaling are not yet configured on either reference machine. Until they are, numbers from here are labelled indicative rather than published, which is a smaller claim than this section will eventually make and an honest one to make today.

Statistics that mean something: medians and interquartile ranges, not means; distributions plotted rather than summarized; enough runs that the confidence interval is narrower than the difference being claimed; explicit warmup for anything measuring steady state, and explicitly no warmup for anything measuring startup.

Everything is versioned, so a number from six months ago can be recomputed against today's build and the difference attributed.

## 15.5.1 The first cold start and memory measurement, and what it does not mean

The moment `katsu run` could execute a file end to end and print from it, the first of the axes in 15.3 became measurable. This is that measurement, taken on `m4`, at the commit that finished the command line interface. Hello world is one `console.log`. A hundred runs per runtime with no warmup, since this is startup, plus fifteen runs under `/usr/bin/time -l` for peak resident set size.

| Runtime | Version | Median wall | p95 | Peak RSS | Binary |
|---|---|---|---|---|---|
| katsu | 0.0.5 | 1.97 ms | 2.35 ms | 2.44 MiB | 1.7 MiB |
| bun | 1.4.0 | 6.09 ms | 7.37 ms | 10.92 MiB | 60.6 MiB |
| deno | 2.9.6 | 12.13 ms | 13.96 ms | 31.09 MiB | 118.9 MiB |
| node | 26.8.1 | 24.67 ms | 27.80 ms | 48.14 MiB | 139.0 MiB |

Against Node that is 12.5 times faster to start, 19.7 times less memory and an artifact 80 times smaller. Against Bun, the runtime that actually competes on this axis, it is 3.1 times faster and 4.5 times less memory.

Now the part that matters more than the table. **We are ahead here because we do less, not because we are better.** There is no module system, no `process`, no event loop, no filesystem, no `Buffer` and no standard library beyond `console.log`. Every one of those costs startup time and resident memory, and Node is carrying all of them in the 24.67 ms above. A fair reading of this table is not that the 10x goal is met, it is that we start from a position where the goal is reachable, and the real test is whether these numbers survive M1 through M8 landing on top of them.

That is exactly why this is recorded now rather than when the runtime is finished. The 4 MiB idle budget in document 02.3 is a budget, and a budget is only useful if you know what you were spending before you started. 2.44 MiB with nothing implemented means the whole Node compatible surface has to fit in the 1.5 MiB that is left, which is a much more demanding statement than the budget looked like in the abstract, and it is better to know that at M0 than at M8.

The usual caveats apply and are worth restating rather than pointing at. `m4` is a laptop with turbo enabled and frequency scaling on, so these are indicative rather than published numbers per 15.5. Wall clock here is measured from outside with `subprocess` around the whole process lifetime, which includes fork and exec and the dynamic loader, and that is the right thing to measure for a cold start even though it is not the runtime's own time. Peak RSS from `/usr/bin/time -l` is what the operating system saw, not what any of these four runtimes would report about themselves.

## 15.6 What benchmarks lie about, and where we say so

The most useful finding in the 2026 runtime comparison literature is also the most inconvenient one for everyone selling a runtime. Marketing benchmarks report Bun at 52,000 requests per second against Node's 14,000, a gap of nearly 270%. When the same comparisons test actual applications with a database and business logic, all three runtimes land at roughly 12,000 requests per second, essentially identical, because routing, validation, database round trips and application logic dominate and the engine becomes noise.

We are going to publish server throughput numbers, so we are going to state that finding next to them. Document 02.5 already caps the server claim at two to four times, and this is the evidence for why it is capped rather than sandbagged. The honest advice, which we will give in our own documentation, is that a five minute benchmark on the user's actual application is worth more than any comparison table, including ours.

Similarly: microbenchmarks measure microbenchmarks. Cold start on a hello world is not cold start on an application. Idle RSS is not RSS under load. Every published number carries the caveat that applies to it rather than a general disclaimer at the bottom of the page.

## 15.7 Regression detection

A subset of the suite runs on every commit on dedicated hardware, with results stored and plotted. A regression beyond a threshold fails the build and names the commit.

The memory budget from document 02.3 is enforced this way per document 08.7, which makes it the only budget in this specification that cannot quietly rot.

Compile time is measured too, both tier 1 and tier 2 compile throughput and AOT build times, because document 09.8 correctly identifies build time as the thing users will complain about and an unmeasured number always gets worse.

## 15.8 The dashboard

Results are public, updated per commit on the main branch, with history. Anyone can see where we are losing.

That is a slightly uncomfortable choice for a project that will spend years being slower than V8 on compute, and it is the right one, because a project that only publishes numbers when they are good teaches everyone to distrust its numbers. Document 13.4 already commits to shipping useful products before the compute story is finished, and being visibly honest about the gap the whole way is what makes the eventual claim believable.
