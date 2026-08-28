# Open questions

Ranked by how much damage a wrong answer does. Each one names what would answer it, when we need the answer, and where we currently lean, because a question with no lean is usually a question nobody has thought about hard enough.

## Q1. Does copy and patch produce competitive code for polymorphic JavaScript operations?

**Why it matters.** Tier 1 is the load bearing assumption of the whole architecture. If generated stencils cannot handle property access, method calls and arithmetic dispatch well, we do not have a cheap baseline JIT, and without a cheap baseline JIT the performance story collapses back to interpreter plus a slow optimizing tier, which is the worst of both worlds.

**Why it is genuinely open.** Xu and Kjolstad's results are on a language with far less polymorphism per operation than JavaScript. Deegen's results are on Lua. Nobody has published copy and patch results for a JavaScript-shaped language with inline caches, shape guards and megamorphic sites, and Haoran Xu's own writeup is blunt that naive stencils produce poor code unless the handlers are restructured into continuation passing style.

**What answers it.** The M2 spike in document 13.3: implement stencils for the twenty most common opcodes including `GetProp` and `Call`, and measure against the interpreter on property heavy real code, not on an arithmetic loop.

**When.** M2, before anything depends on it.

**Lean.** It works, but only with the full set of techniques in document 06.2: CPS structured handlers, hot and cold splitting, inline cache slabs, and register pinning. The risk is not that it fails, it is that it takes twice as long as planned to get right.

**If the answer is no.** Hand write tier 1 for the top thirty opcodes and generate the rest. More work, less elegant, still viable.

## Q2. Cranelift or our own backend for tier 2?

**Why it matters.** Months of work and the entire deopt design hang on it, and the IR is shaped by the answer.

**Why it is open.** Cranelift has user stack maps, where the producer declares live values at a safepoint through `declare_needs_stack_map` and `append_user_stack_map_entry`, and spills become ordinary IR loads and stores the optimizer can see. That is the right foundation. What could not be found is any evidence of deoptimization support: tier down, deopt points, frame reconstruction. The absence of published results is informative but not conclusive.

**What answers it.** Build the deopt path on Cranelift stack maps at M6 and see whether it works and what it costs. Not a documentation question.

**When.** M6, and the IR carries a backend trait until then so both stay implementable.

**Lean.** Cranelift, because saving months matters more than the last few percent of code quality at this stage, and because our own backend is exactly the code that produces memory corruption CVEs.

## Q3. MMTk or Whippet?

**Why it matters.** The memory goal and the pause time story both run through it, and it is the largest single third party dependency in the runtime.

**Why it is open.** MMTk is Rust, which makes it the default, with production bindings including Ruby 3.4's modular GC. Whippet's Nofl collector pushes reclamation granularity down to allocator alignment and reports beating copying and mark sweep collectors at tight to adequate heap sizes, which is precisely our situation under document 02.3's budget. But Whippet is C and costs an FFI layer.

**What answers it.** M4: integrate both behind the interface in document 08.2 and measure throughput on our own allocation traces, pause distribution, RSS at a tight heap, ephemeron support, and the FFI cost.

**When.** M4.

**Lean.** MMTk, on the strength of being Rust and having production users, unless the tight heap numbers are decisively Whippet's.

## Q4. Which interpreter dispatch strategy?

**Why it matters.** Interpreter speed is cold start speed, and cold start is the axis we make the loudest claim about.

**Why it is open.** `become` is nightly only, documented as incomplete, and the Trifecta Tech project goal targets stabilization in 2027 contingent on funding. We cannot depend on it. What is not known is how much a stable `loop { match }` actually loses on modern branch predictors, or whether stencil threading recovers it.

**What answers it.** Implement all three per document 05.3 and measure at M3. `#[loop_match]` in rustc is worth tracking as a fourth option.

**When.** M3.

**Lean.** `loop { match }` is closer to the others than folklore suggests, stencil threading wins by enough to be worth its complexity given that we build stencils anyway, and this ends up mattering less than Q1 does.

## Q5. How deep is the Node-API iceberg?

**Why it matters.** It is the gate on the compatibility claim. Document 13.6 says that if it fails, the product narrows from "runs Node programs" to "runs pure JavaScript Node programs", which is materially smaller.

**Why it is open.** Node-API is explicitly designed to be engine independent and the Node-API team maintains documentation on binding it to other engines, so the design is on our side. But Bun still requires a Node process for `.node` addons, and Deno only added native addon support in Deno 3. Two well funded teams found this hard.

**What answers it.** Implement enough of the API at M8 to load `better-sqlite3` and `sharp` and pass their test suites. Those two exercise external buffers, finalizers, threadsafe functions and object wrapping, which is most of the interesting surface.

**When.** M8, but a two week feasibility read of the header against our object model should happen much earlier, because a surprise here is expensive.

**Lean.** It works, and finalizer semantics under a moving collector plus `node-gyp` build integration are the two places it hurts.

## Q6. Is 4 MB idle actually achievable?

**Why it matters.** It is half the headline claim.

**Why it is open.** The line item budget in document 02.3 adds up on paper: 1.5 MB of binary pages, 400 KB of snapshot, 200 KB of atoms and shapes, 512 KB of initial heap, and so on. Budgets that add up on paper routinely fail in practice because of allocator overhead, page granularity, and the twenty small things nobody budgeted for.

**What answers it.** The heap census command and the CI RSS test from document 08.7, running from M1 so that the number is watched from the beginning rather than measured at the end.

**When.** Continuously. The first real reading is at M4 when the collector is real.

**Lean.** Achievable for a minimal build, tight but achievable with the Node layer, and the risk is that the Node layer's own state is larger than the 300 KB budgeted for it.

## Q7. Is the tagged pointer object model right, or does Nova's index design deserve another look?

**Why it matters.** It is the hardest thing to change later.

**Why it is open.** Document 07.3 rejects index based references because they add an indirection on the JIT's hottest path that the JIT cannot optimize away. That reasoning is sound but it is reasoning, not measurement, and Nova is interpreter only so nobody has data on how an index based heap behaves under a JIT.

**What answers it.** Honestly, nothing we are going to do, because prototyping both object models is a quarter we do not have. This question stays open as a documented decision with its rationale, revisited only if the M2 spike turns up something that changes the analysis.

**When.** M2 informs it. Otherwise it is closed by decision rather than by evidence, and this document says so rather than pretending otherwise.

## Q8. How much real TypeScript is typed enough for AOT to pay?

**Why it matters.** Typed compute is the one axis where document 02 claims 10x or better, and that claim rests on real programs having enough provable types.

**Why it is open.** Static Hermes's 300x numbers are microbenchmarks on fully typed code. Perry reports integer recursion within a couple of percent of Rust, but Perry trusts annotations, which document 09.4 refuses to do. Our guarded speculation is cheaper than dynamic dispatch and more expensive than trusted types, and nobody has measured that middle ground on real applications.

**What answers it.** A study, runnable before M9 and cheap: take fifty popular TypeScript packages, run our inference over them, and report the fraction of operations landing in Proven, Speculated and Dynamic.

**When.** Before M9, and it is cheap enough to do far earlier. It should probably happen during M1 as a paper study, because the answer shapes how much AOT work is worth doing.

**Lean.** Numeric and data processing code is heavily typed and wins enormously. Application glue is mostly Dynamic and wins little. The honest claim is therefore about kernels rather than about programs, which is already how document 02.4 words it.

## Q9. Is oxc the right frontend dependency, and is a separate pre-parse pass worth it?

**Why it matters.** It is the largest external dependency in the hot path of cold start.

**Why it is open.** oxc publishes 26.3ms against SWC's 84.1ms on typescript.js, passes all test262 stage 4 tests, and is maintained under VoidZero with Rolldown and Vite depending on it, so the project is real. The open part is whether it is fast enough that document 04.6's separate pre-parse pass is a pessimization rather than an optimization, and whether the AST it produces costs us more in adaptation than a purpose built parser would.

**What answers it.** Implement both laziness strategies behind a flag and measure, per document 04.6.

**When.** M1.

**Lean.** oxc is right and stays right. The pre-parse pass is probably worth it for large bundles and probably not for small programs, which means it becomes a heuristic on input size rather than a fixed choice.

## Q10. What do we do about Intl?

**Why it matters.** ECMA-402 is thousands of test262 tests and an ICU dependency measured in tens of megabytes, which directly contradicts the distribution size claim in document 02.4.

**Why it is open.** ICU4X is Rust and modular and is clearly the right answer in principle. What is not known is how much of the distribution size budget a useful subset actually costs, and whether data slicing produces something that passes enough of ECMA-402 to be worth claiming.

**What answers it.** Build it with ICU4X at several data configurations and measure both binary size and ECMA-402 pass rate.

**When.** M3, alongside the conformance push.

**Lean.** Ship two builds, report `Intl` support honestly per build, and never stub `Intl` with something that returns plausible wrong answers.

## Q11. Does anyone actually need the raw V8 API?

**Why it matters.** Document 10.6 says no for 1.0. If that is wrong, a meaningful set of packages simply does not work and we find out from users.

**Why it is open.** Bun built a V8 API shim on JavaScriptCore for the addons its users needed, which proves it is possible and suggests the demand was real enough to justify the cost. We have no data on which packages in the top ten thousand actually bypass Node-API.

**What answers it.** A survey. Download the top ten thousand packages with native components, and check which link against V8 symbols directly. This is a scripting job, not a research project.

**When.** Before M8, so the answer informs how the Node-API work is scoped.

**Lean.** The list is short and dominated by a handful of important packages, which would make a narrow shim worth funding after 1.0.

## Q12. Who is going to build this?

**Why it matters.** Document 13.5 puts the total at 55 to 90 engineer-months to 1.0, which is three to five years for a team of two, and the work does not parallelize well below about four people because the pieces are deeply coupled.

**Why it is open.** Because it is a resourcing question, not a technical one, and because every ambitious runtime project that failed did so here rather than on a technical gate.

**What answers it.** Nothing in this specification. What this specification can do is make the four stopping points in document 13.4 real, so that the project produces something useful at each of them regardless of how long the funding lasts.

**When.** Now, before M0.

**Lean.** Build toward M4 and M7 as products in their own right, and treat everything past M9 as contingent.
