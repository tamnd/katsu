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

Statistics that mean something: medians and interquartile ranges, not means; distributions plotted rather than summarized; enough runs that the confidence interval is narrower than the difference being claimed; explicit warmup for anything measuring steady state, and explicitly no warmup for anything measuring startup.

Everything is versioned, so a number from six months ago can be recomputed against today's build and the difference attributed.

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
