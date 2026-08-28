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

### 14.2.1 The runner as built, and the first number

The runner exists and it reports 6.65%. That is 5,405 passing cases out of the 81,225 it attempted, from 53,580 test files that expand into 92,709 cases once both modes are counted, against suite revision `ac7b5f8` taken on 28 August 2026, in 2.2 seconds on an M4. The number is small and it is real, which is the only property that mattered for the first one.

The report separates three things that a single pass rate would add together. 5,405 passed. 7 failed. 75,813 reached a construct this build has not implemented, and 11,484 were not run at all. A failure is a bug and an unsupported case is a piece of work, and a combined figure would be neither, so the report prints them apart and prints the rate against everything attempted with the skip count next to it. Skips stay in the total with their reason printed, because a skip that leaves the denominator is how a conformance number becomes decoration.

The most important decision in the runner is the one that keeps the number from being flattering. Roughly a tenth of the suite is negative tests, files that are supposed to be rejected, and this build rejects nearly everything. A runner that cannot tell "this is invalid JavaScript" from "this is valid JavaScript the engine has not built yet" credits every one of those as a pass, which inflates the rate by exactly the amount of work remaining. So `ParseError` grew an `is_not_implemented` predicate, `katsu-api` classifies compile failures into `Error::Syntax` and `Error::NotImplemented` rather than collapsing both into the first, and the runner refuses to count the second as a pass on a negative test. `/./u` is reported as an unimplemented regular expression and scores nothing, while `/./uv` is reported as the `u` and `v` flags being enabled at the same time and scores a pass, which is the distinction working exactly where it is hardest to see.

The phase is checked on a negative test and the error name is not, because every error found before anything runs is a syntax error today, the parser having one error type. That is sound only because all 4,657 parse phase negative tests in the suite expect a `SyntaxError`, which was checked against the suite rather than assumed. It stops being sound the day one of them expects something else, and the fix then is an error kind on `ParseError` rather than a special case in the runner.

The expectations file records what passes rather than what fails, which is the opposite of what 14.2 above describes and the change is deliberate. Both directions are a ratchet and they fail differently. Recording failures means a test that quietly stops running at all, because a skip rule grew or a file was renamed, looks exactly like a fix. Recording passes means that same test drops out of the set and is reported as a regression, which is the correct answer, because a test we no longer run is a test we no longer know the answer to. A case that starts passing also fails the check until it is committed, so an improvement has to be banked rather than left as slack in the ratchet. The file records the suite revision it was taken against, since the suite gains and renames tests every week and a cross revision diff is indistinguishable from a real regression unless something says so.

The first run found two things worth having. 75,804 of the 75,813 unsupported cases stop at the same construct, a single `switch` statement on line 25 of `harness/assert.js`, which every non `raw` test in the suite has prepended to it. One statement form is standing between this engine and the ability to attempt most of the suite, and that is a fact about where to spend the next week that no amount of reading the code would have produced. The 7 failures are all the same finding too: Annex B allows a call expression as an assignment target and our parser rejects it as an early error, so we refuse seven programs that are valid.

Any one case gets five seconds before a watchdog thread asks it to stop through the interrupt flag from 5.6, which the interpreter checks on every loop back edge. One thread watches all of them rather than one thread per case. What that does not cover is a program stuck in straight line code, which has no back edge to check at, so the real bound there is the program's length rather than the timeout.

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

### 14.5.1 The harness as built, and what it found

There is one tier, so tier against tier would compare the interpreter with itself and catch nondeterminism and nothing else. Rather than let the harness wait for M6 to have a second thing to compare against, the second differential above was built first: katsu against node, on generated programs. That is a strictly harder question than tier against tier, it is the question the compatibility goal is actually about, and the machinery is the same machinery. When the tiers arrive they become two more oracles behind the same trait and the generator, the shrinker and the report do not change.

The generator emits only the subset the interpreter runs today, because a program that stops at the first unimplemented construct tests nothing. Its literal tables are not random digits. Every value in them is a known place where two implementations of ECMAScript stop agreeing: `1e21` and `1e-7` are the two ends of where number to string switches to exponential notation, `2147483648` is one past what the bitwise operators truncate to, `9007199254740993` is the first integer a double cannot hold, and `"10"` against `"9"` is the comparison that catches an implementation comparing strings numerically. Random digits find none of these, because the interesting inputs are a vanishingly small part of the space.

Three decisions carry most of the value. Unimplemented constructs are their own verdict rather than a disagreement, because katsu not having an opinion is not katsu and node disagreeing, and without that separation the report becomes the work list wearing the word "divergence" and nobody keeps reading it. Thrown errors are compared on the constructor name and not the message, because the standard specifies which error is thrown and says nothing at all about what it says. Two engines both refusing to parse a program is agreement for the same reason, which cost five false reports before it was fixed. Every divergence is shrunk by removing statements until nothing more can go, so a finding arrives as three lines rather than as forty.

The first run against node found three real bugs in a thousand programs, none of which any test in the repository was going to find:

`undefined` was not a binding. It is not a keyword, it is an ordinary property of the global object, and `typeof` of an unresolvable reference is defined to return the string "undefined" rather than to throw. So `typeof undefined` answered correctly in an engine that had never heard of the name while `let x = undefined` threw a `ReferenceError`, and that asymmetry is exactly why every obvious test passed. `NaN` and `Infinity` were missing for the same reason and went in with it.

`console.log(-0)` printed `0`. Negative zero is the one number whose inspected form and whose `ToString` differ, and the fix had to go in the inspection path only, because `'' + -0` is required to be `"0"` and a console that cannot tell you which zero you are holding is hiding the thing you turned it on to see.

The third was a code generation bug. Lowering documents a rule that a destination register is never passed down to an operand, because an operand is evaluated before its siblings and writing the destination early clobbers a variable a later operand still reads. The short circuiting operators broke that rule: `v = 1.5 && ('x' + v)` built the left side in `v`'s own slot and then read the new value back, answering `x1.5` where node says `x3`. The compound assignment form of the same operator had a comment saying the result has to end up in a register that is nobody's variable, and was correct. This was the same problem one match arm over, and it is a good example of the class: a bug that no amount of reading finds and that a hand written test only finds if somebody already suspects it.

A longer run of twenty thousand programs then found a fourth, and it is the one that says most about why this is worth running. `9007199254740993 / 10` printed `900719925474099.3` where node prints `900719925474099.2`. Both engines had the length right and the digits right up to the last one. The double is exactly `900719925474099.25`, so the two sixteen digit candidates either side of it are exactly the same distance away, and the standard's step five says that when that happens you take the even one. Rust's shortest formatting takes the larger one. This class of value is about five percent of the doubles a sweep runs into, it is invisible to every test written by hand because writing one means already knowing the rule, and no amount of reading the code finds it because the code was correct about everything it knew it had to be correct about. The fix keeps taking the digits from Rust and corrects the last one, with the tie confirmed by writing the value out exactly, since a double needs at most seven hundred and sixty seven significant digits and that is a bound rather than an estimate.

With those fixed, the generated programs and the corpus agree with node on every one. CI runs two thousand at a fixed seed on every commit, which is a reproduction rather than a flake, and finding new things is what longer runs on the reference machines are for.

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
