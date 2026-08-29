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

The runner exists and it reports 6.66%. That is 5,410 passing cases out of the 81,225 it attempted, from 53,580 test files that expand into 92,709 cases once both modes are counted, against suite revision `ac7b5f8` taken on 28 August 2026, in 2.4 seconds on an M4. The number is small and it is real, which is the only property that mattered for the first one.

The report separates three things that a single pass rate would add together. 5,410 passed. 7 failed. 75,808 reached a construct this build has not implemented, and 11,484 were not run at all. A failure is a bug and an unsupported case is a piece of work, and a combined figure would be neither, so the report prints them apart and prints the rate against everything attempted with the skip count next to it. Skips stay in the total with their reason printed, because a skip that leaves the denominator is how a conformance number becomes decoration.

The most important decision in the runner is the one that keeps the number from being flattering. Roughly a tenth of the suite is negative tests, files that are supposed to be rejected, and this build rejects nearly everything. A runner that cannot tell "this is invalid JavaScript" from "this is valid JavaScript the engine has not built yet" credits every one of those as a pass, which inflates the rate by exactly the amount of work remaining. So `ParseError` grew an `is_not_implemented` predicate, `katsu-api` classifies compile failures into `Error::Syntax` and `Error::NotImplemented` rather than collapsing both into the first, and the runner refuses to count the second as a pass on a negative test. `/./u` is reported as an unimplemented regular expression and scores nothing, while `/./uv` is reported as the `u` and `v` flags being enabled at the same time and scores a pass, which is the distinction working exactly where it is hardest to see.

The phase is checked on a negative test and the error name is not, because every error found before anything runs is a syntax error today, the parser having one error type. That is sound only because all 4,657 parse phase negative tests in the suite expect a `SyntaxError`, which was checked against the suite rather than assumed. It stops being sound the day one of them expects something else, and the fix then is an error kind on `ParseError` rather than a special case in the runner.

The expectations file records what passes rather than what fails, which is the opposite of what 14.2 above describes and the change is deliberate. Both directions are a ratchet and they fail differently. Recording failures means a test that quietly stops running at all, because a skip rule grew or a file was renamed, looks exactly like a fix. Recording passes means that same test drops out of the set and is reported as a regression, which is the correct answer, because a test we no longer run is a test we no longer know the answer to. A case that starts passing also fails the check until it is committed, so an improvement has to be banked rather than left as slack in the ratchet. The file records the suite revision it was taken against, since the suite gains and renames tests every week and a cross revision diff is indistinguishable from a real regression unless something says so.

The first run found two things worth having. 75,804 of the 75,813 unsupported cases stop at the same construct, a single `switch` statement on line 25 of `harness/assert.js`, which every non `raw` test in the suite has prepended to it. One statement form is standing between this engine and the ability to attempt most of the suite, and that is a fact about where to spend the next week that no amount of reading the code would have produced. The 7 failures are all the same finding too: Annex B allows a call expression as an assignment target and our parser rejects it as an early error, so we refuse seven programs that are valid.

Two milestone items later the wall has moved twice and not fallen, which is itself the result. Implementing `switch` moved it to the `try` two lines below, and implementing `try` moved it along that same line to the `new`, because the line in full is `throw new Test262Error(message)` and the suite reaches it before it can report anything. 75,807 cases now stop there, and the construct standing in front of five sixths of the suite is a constructor call, which needs prototypes rather than another statement form. That is worth knowing because it is the first time the answer to "what unlocks the suite" has been a piece of the object model instead of a piece of the grammar, and it changes what the next week is spent on.

Making `throw` run also turned five cases from unsupported into failures, which is the ratchet working rather than a regression: the recorded pass set did not move, and five programs that used to stop at a `throw` now run past it into a place where we do not raise an early error we owe. All five are strict mode identifier rules. `var public = 1` under `"use strict"` is a `SyntaxError` before anything runs and so is `eval = 42`, and we accept both, so the fifth failure category in the report is the work list entry that comes with it.

Implementing `finally` moved nothing at all, and that is worth recording rather than leaving to be inferred. Every count above is identical after it, because the wall is the `new` on that same line and a construct behind the wall cannot show up in the number until the wall falls. It is the first piece of work here whose conformance value is entirely deferred, and the lesson is about the metric rather than the work: a pass rate against a suite that stops at one construct measures the distance to that construct and nothing else, which is why the report prints the work list next to the rate.

The strict mode identifier rules closed that entry and the five cases went straight from failures to passes, so the report is back to 7 failures and all seven are the Annex B assignment target finding it opened with. They are worth naming because they are the smallest useful piece of work this project has done: five test262 cases, two error messages, one list of nine words, no new opcode and no change to any pass below the adapter. The pass count moved by five out of 81,225, which is not the point. The point is that the ratchet turned an implementation decision made two milestones ago into five named cases and then watched them close, without anybody having to remember they were owed.

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

With those fixed, the generated programs and the corpus agree with node on every one. CI runs two thousand at a fixed seed on every commit, which is a reproduction rather than a flake, and finding new things is what longer runs on the reference machines are for. Three thousand agreed on every program the day `try` and `catch` were added to the generator.

The generator grew a `try` production with the exceptions work, and the two decisions in it are worth naming because a careless version of either would generate programs that cannot fail. The throw is drawn rather than always emitted, because a `try` that always fires never runs the path where the protected block finishes and that is the path almost every real `try` takes. And the handler assigns the caught value to a binding declared outside the `try`, because a generated handler mentions the caught name only by accident, so without somewhere to put it a program would print the same thing whichever path it took and a difference in where a throw landed would never reach the output being compared.

`finally` extended that production rather than adding another, and the extension has a decision of its own. Which of the two clauses a `try` gets is drawn, since the three shapes lower into three different things, and a `try` with no `catch` is only drawn when no throw was drawn with it, because a throw nothing catches ends the program and a generator that ends its programs early stops testing everything written after that point. Both clauses then write to the same counter, the `catch` by assigning the caught value and the `finally` by adding to whatever is there, which is one binding instead of two and puts the order the clauses ran in into the output rather than only the fact that they ran. Three thousand programs agreed on every one the day `finally` landed.

The loop work turned the one loop production into three, drawing between a `while`, a `do while` and a C style `for` rather than always emitting the `while` the generator started with. Only the `for` keeps its counter in the head, because its update runs on the way round even after a `continue` skipped the rest of the body, while the other two keep the increment as the first statement of the body for the reason that production has always had, which is that a `continue` past an increment at the bottom is a loop that never ends and a generator killed by timeouts. A `do while` goes round one more time than the other two for the same bound, and that asymmetry is the point of drawing all three rather than one, since an engine can put the test in the wrong place and still get every `while` right. A hand written corpus file went in beside the generated programs for the parts a generator will not reach on its own: an empty head, a `continue` in each of the three forms, a `finally` that a `continue` and a `break` pass through, a `var` head that outlives its loop next to a `let` head that does not, and the head reading its own name before it has a value. Twenty thousand generated programs and the corpus agreed with node on every one, in nineteen minutes on an M4.

Two of the loop cases were worth measuring rather than assuming. A `for` head is in its own dead zone while its initialiser runs, so `for (let i = i; ;)` is a `ReferenceError` and not a read of an outer `i`, and a `const` head with no initialiser is refused by the parser before scope analysis sees it, which means the check written for it in the scope pass was unreachable and came back out. The loop forms moved the test262 number by nothing at all, and that is the expected answer rather than a disappointment: 75,807 cases stop on the `new` in the suite's own harness before the test body is reached, so a filtered run of `language/statements/for` reports the same 75 passing with the loops implemented as without them. The suite does not get to see this work until constructors land.

Labels gave the generator two more productions and one more piece of state. Loops now carry a label about half the time, a labelled block is drawn as a statement in its own right, and the jump production draws a name from what is in scope about half the time it has one to draw. The state is two lists rather than one, because what a label denotes is decided where it is written and not where the jump is: every labelled statement is something a `break` can name and only the ones on loops are something a `continue` can name, so filtering on the way out would have to know what each label was on and keeping two lists does not. A labelled `continue` is as safe from a runaway loop as an unlabelled one for the same reason the unlabelled one is safe, since every loop form the generator emits moves its counter somewhere a `continue` cannot skip, and that holds for the outer loops a labelled `continue` reaches as well as the nearest one. Across two hundred seeds the generator produced 465 labelled statements, 65 labelled breaks and 6 labelled continues, including a labelled `continue` written inside a `finally`, which is the shape the design of the completion token exists for. Seven thousand generated programs and the corpus agreed with node on every one.

The hand written label corpus covers what the generator will not reach: a labelled `continue` and a labelled `break` out of a nested loop next to the unlabelled versions of both for comparison, a labelled block used as an early exit with no loop involved, a chain of two labels on one loop, a labelled `break` out of a `switch` inside a loop where the unlabelled one leaves only the switch, two different labelled breaks through one `finally`, a labelled `break` through two nested `finally` clauses, a labelled `continue` through a `finally`, a label on a `do while`, a label sharing a name with a live variable, and the same label name reused beside itself and again inside a nested function. Labels moved the test262 number by nothing at all for exactly the same reason the loops did: a filtered run of `language/statements/labeled` reports the same 15 passing and 21 unsupported before and after, and every one of the 21 says `new is not supported yet`.

One thing labels found was a crash rather than a divergence. Annex B lets sloppy code write a function declaration as the single statement of an `if` or under a label, and node runs `if (c) function f(){}` and `l: function f(){}` while refusing both in strict mode. katsu panicked on all of them, because nothing declared the name and the scope pass then looked it up, and the panic predated labels by two milestones. It is refused by name now, as a construct the frontend does not support, which is an honest answer where a crash was telling the user nothing about their program.

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
