# The quality bar

## 14.1 Three claims, three kinds of evidence

The project makes three claims that a user has to be able to check. The language is complete, which test262 measures. Node programs run unmodified, which Node's own test suite and a real package corpus measure. And the runtime is safe, which fuzzing, sanitizers and an external review measure.

Performance is the fourth claim and it has its own document. This one is about being correct enough that the performance numbers mean anything.

## 14.2 test262

test262 is the ECMAScript conformance suite and it is the only number anyone in this field respects. The public scoreboard is test262.fyi, which runs engines nightly, and being on it is a goal in its own right because it makes the claim externally verifiable rather than self reported.

The bar the Rust field has set: **Boa reports 95.5% as of 2026, roughly 51,000 tests of about 53,000, which places it fourth and above JavaScriptCore.** Boa got there over several years, from 87.3% at v0.20 through 94.12% at v0.21, and its maintainers have publicly shifted focus toward performance now that conformance is approaching a ceiling. That trajectory is the realistic shape of this work and our milestones in document 13 are drawn to match it: 80% at M1, 95% at M3.

Mechanically:

Our own runner, in tree, running the suite in parallel in both strict and sloppy modes, honoring the metadata including `negative`, `features`, `includes`, `raw` and `module`.

An expectations file listing every currently failing test, checked into the repository. A test that starts passing must be removed from the file or CI fails, and a test that starts failing fails CI immediately. **This ratchet is the single most valuable piece of test infrastructure in the project**, because it converts conformance from a number somebody checks occasionally into a property the build enforces.

Every tier runs the suite. A test passing in the interpreter and failing under tier 2 is a JIT bug and is exactly the class of thing that ships in other engines.

Staged proposals are opt in behind flags and their failures do not count against the headline number, reported separately so the number stays honest.

`Intl` is a decision rather than a detail. The full ECMA-402 surface is thousands of tests and a dependency on ICU, which is tens of megabytes and directly contradicts the distribution size goal in document 02.4. The plan is ICU4X, which is Rust and modular, with a build without it that reports `Intl` as absent rather than lying, and the ECMA-402 tests reported as a separate line.

## 14.3 Node's own test suite

This is what Bun does to measure compatibility and it is the only measurement worth publishing.

We run Node's `test/parallel` and `test/sequential` suites, report pass rate per module, and publish the failures. Document 10.8 sets that as the compatibility number. A summary percentage with no failure list is marketing, so the failure list ships with the number.

The same ratchet applies. Known failures are in a checked in file with a reason attached, and the reasons fall into categories we can count: not implemented, intentional divergence, depends on a V8 internal, or a real bug. Watching the "real bug" count go to zero while the "intentional divergence" count stays stable and documented is what compatibility work looks like from the outside.

## 14.4 The real world corpus

Passing standard library tests and failing to run Express are different kinds of success, so a corpus of actual packages is installed and exercised in CI, weighted by npm download counts.

Frameworks that must run their own test suites: Express, Fastify, Koa, Hono, Nest. Tooling: Vite, esbuild, Rollup, webpack, TypeScript itself, ESLint, Prettier. Test runners: Jest, Vitest, Mocha, `node:test`. Native addon dependents: `better-sqlite3`, `sharp`, `bcrypt`, `canvas`, `node-gyp` built packages generally, since document 13's M8 gate depends on them. Data and infrastructure: `pg`, `mysql2`, `ioredis`, `mongodb`, `ws`, `undici`.

Running someone else's test suite against your runtime is a brutal and extremely effective test, because those suites were written by people who cared about edge cases in that specific domain and had no idea you existed.

## 14.5 Differential testing, which is our structural advantage

The interpreter is the reference implementation. Every tier must agree with it on every program.

A differential harness generates or mutates programs, runs each under `--tier=interp`, `--tier=baseline` and `--tier=optimizing` with tier up forced early, and compares the full observable output: return values, thrown exceptions and their types, side effect ordering, and the sequence of property accesses on an instrumented Proxy.

Any disagreement is a bug in one of them, and because the tiers are generated from one semantic description per document 05.2, a disagreement is usually a bug in the generator, which means one fix repairs a class rather than an instance.

This deserves emphasis: **most JIT bugs in production engines are only detectable when they cause a crash or a memory safety violation.** A JIT that computes the wrong number silently is invisible to a fuzzer looking for segfaults. Our oracle catches wrong answers directly, and that is a materially stronger position than the engines we are competing with, purely because we chose to keep an interpreter that is a real reference rather than a legacy tier.

A second differential runs against Node for behavior test262 does not pin, particularly error messages, stack trace shapes, and loop ordering, feeding the divergence list in document 10.7.

## 14.6 Fuzzing

The field has converged on structure aware, coverage guided fuzzing of JavaScript engines and the tooling is public.

**Fuzzilli** is the baseline: a coverage guided fuzzer built on FuzzIL, a custom intermediate language that is mutated at the level of control and data flow rather than syntax, then lifted to JavaScript. Its NDSS 2023 paper reports 17 confirmed security vulnerabilities in a six month evaluation, and it has been credited with 51 bugs across six engines. We write a Fuzzilli target, because it is the standard and because it means anyone in the security community can point it at us with no work.

The known weakness is that general purpose fuzzers struggle to reach the JIT at all, which is why the successors exist and why we track them rather than stopping at Fuzzilli. **FuzzJIT** (USENIX Security 2023) adds an input wrapping module and a JIT specific mutation strategy on top of Fuzzilli. **OptFuzz** (USENIX Security 2024) guides by optimization path coverage rather than code coverage. **BCFuzz** (ASE 2025) drives from bytecode. **TYPEFUZZ**, a registered report at the NDSS 2026 fuzzing workshop, argues code coverage is the wrong signal for JIT bugs, citing a Turbofan type confusion that coverage guided campaigns did not find, and instead tracks heap object types observed at optimization sensitive locations as the feedback signal.

That last one is the most directly relevant, because type confusion at speculation sites is precisely our risk surface, and because our IR knows where its optimization sensitive locations are without needing clang-tidy to find them. Instrumenting our own speculation sites for type feedback coverage is cheap for us and expensive for V8, which is an advantage worth taking.

Alongside that, ordinary `cargo-fuzz` targets on the parser, the bytecode deserializer, the snapshot loader, the regular expression engine, JSON, and every format that crosses a trust boundary.

## 14.7 Unsafe code

Document 03.2 sets the rule: `katsu-builtins`, `katsu-node` and `katsu-api` are `#![forbid(unsafe_code)]`. Unsafe exists in the collector, the value representation, the code buffers, the stack, and the platform layer, and nowhere else.

Every `unsafe` block carries a `// SAFETY:` comment stating the invariant. This is enforced by a lint, not by review discipline.

Miri runs over the test suite for everything it can execute, which is most of the runtime except the generated machine code. ASan, UBSan and TSan builds run in CI. `cargo-geiger` reports the unsafe surface per crate on every release and the number going up is a conversation.

The total count of unsafe blocks is a published number, tracked over time, because it is the honest measure of how much of Rust's guarantee we are actually keeping. An engine that is "written in Rust" with unsafe scattered through fifty thousand lines has bought very little.

## 14.8 JIT security

A JIT is a machine that turns attacker influenced input into executable code, which is why JIT bugs are the most valuable exploit class in browsers.

The mitigations, most of which are specified elsewhere and collected here: the cage from document 07.2 so that a corrupted reference is an offset into our own heap rather than a process wide read and write primitive; the external pointer table so that native pointers are never forgeable from inside the heap; W^X with the RAII discipline from document 06.9 so that writable and executable never overlap in time on a thread; guard regions around the cage so out of bounds indexing faults; and constant blinding for large immediate values in generated code, so an attacker cannot plant a chosen instruction sequence as a constant and jump into the middle of it.

Deoptimization correctness is a security property, not just a correctness one, because a frame state that describes the wrong layout writes attacker influenced values into the wrong slots. The differential harness in 14.5 runs with forced deoptimization at every guard as one of its modes.

`--jit=off` exists and is a supported configuration, for users who would rather be slow than exposed. It is also what document 09 already produces for AOT targets with no writable executable memory.

An external security review of the JIT, the object model and the cage is an explicit M11 deliverable in document 13, and a security policy with a disclosure process exists before 1.0 rather than after the first report arrives.

## 14.9 Reproducibility

Every bug report has to be reducible to a command someone else can run.

`--tier=` pins execution to one tier. `--jit=off` disables both. `--deopt-every=N` forces deoptimization at every Nth guard. `--gc-stress` collects at every allocation site. `--seed=` fixes anything nondeterministic we control. A one line reproduction with a tier flag is the difference between a bug that gets fixed this week and one that sits open for a year.

Builds are reproducible, including the stencil library from document 06.3, which has its own CI job regenerating it and failing if the output differs from what is committed.

## 14.10 Soak and stress

From M5 onward, a long running job serves traffic for hours and watches RSS, heap size, code cache size, pause times and throughput for drift. Fragmentation, code cache pathologies, leaked handles and finalizer bugs do not show up in a unit test, and document 08.8 names fragmentation under long running server load as the most likely place our collector embarrasses us. The soak test is where we find out first instead of a user finding out.

Stress modes that run in CI on a schedule rather than per commit: collection at every allocation, tier up at one invocation, deoptimization at every guard, a 1 MB heap, and the code budget set to something absurdly small so eviction runs constantly. Each of these turns a rare interleaving into a common one.

## 14.11 The platform matrix

Tier 1, tested on every commit and blocking a release: Linux and macOS on x86-64 and aarch64, and Windows on x86-64.

Windows is tier 1 rather than tier 2 because the machines we develop and benchmark on include one, and because the cost of keeping it working is a four function seam in `katsu-platform` rather than a portability layer smeared across the runtime. The thing that makes a platform expensive is discovering it is broken six months later, so it is in the test matrix from the beginning. Windows on aarch64 is tier 2 until there is a runner for it.

Tier 2, tested per release: Windows on aarch64, Linux with musl, FreeBSD.

Tier 3, best effort, no promises: everything else, including 32 bit targets where pointer compression is meaningless and the cage has to be disabled.

Being explicit about this in advance stops the slow drift where a platform is half supported and nobody knows whether a bug on it is a release blocker.
