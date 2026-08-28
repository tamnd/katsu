# Architecture

## 3.1 The shape of it

```
                    source (.js .mjs .cjs .ts .mts .cts .jsx .tsx)
                                       |
                            katsu-parse (oxc + our lowering)
                                       |
                                katsu-ir : bytecode
                                       |
              +------------------------+------------------------+
              |                                                 |
        JIT mode: katsu run                          AOT mode: katsu build
              |                                                 |
     +--------+--------+                                 emit a Rust crate
     |        |        |                                        |
   T0 interp T1 base  T2 opt                               cargo build
     |        |        |                                        |
     +--------+--------+                              one static binary that
              |                                       embeds T0 for deopt
              +------------------------+------------------------+
                                       |
              katsu-vm      values, shapes, inline caches, frames
              katsu-gc      collector binding, barriers, roots, the cage
              katsu-builtins  the ECMAScript library
              katsu-node    node:* modules, resolution, Node-API host
              katsu-loop    event loop, io_uring / kqueue, timers
                                       |
              katsu-api     the Rust embedding and interop surface
```

The structural claim that everything else depends on: **the two modes diverge at code generation and nowhere else.** Same parser, same bytecode, same object model, same collector, same builtins, same Node layer. An AOT compiled program and a `katsu run` of the same program call the identical `Array.prototype.sort`. If a program behaves differently between modes, that is a bug with an owner, not a documented trade off.

## 3.2 Layers

```
L5  katsu            the CLI, the two drivers, config, the profiler
L4  katsu-node       node:* modules, module resolution, Node-API host
    katsu-api        the Rust embedding and interop API
L3  katsu-builtins   ECMAScript library, Intl, RegExp glue
L2  katsu-jit        stencil assembler (T1), SSA IR and optimizer (T2)
    katsu-aot        Rust source emission
L1  katsu-vm         Value, Object, Shape, InlineCache, Frame, T0 interpreter
    katsu-loop       event loop, timers, I/O
L0  katsu-gc         collector binding, allocation, barriers, root scanning
    katsu-ir         bytecode definition, the opcode DSL, IR types
    katsu-parse      lexer wrapper, scope analysis, AST to bytecode lowering
```

Two rules that make the Rust choice actually pay off.

**L3 and above contain no `unsafe`.** Builtins and the Node layer are written against `Value`, `Handle` and the object protocol, with `#![forbid(unsafe_code)]` at the crate level. That is where most of the code volume lives and most of the historical CVEs in other engines live, and making it safe by construction is the whole reason to write this in Rust. We do not trade that away for three percent on `Array.prototype.indexOf`.

**Below L2, every `unsafe` block carries a comment naming the invariant it depends on**, and document 14 makes that a review gate rather than a convention. The unsafe surface is the collector, the object model's raw slot access, and the JIT's code buffers. Nothing else.

## 3.3 A program through JIT mode

1. **Resolve.** Node's resolution algorithm finds the entry file. On a warm resolution cache this is a hash check and one file read instead of dozens of stat calls, which is the startup lever from document 02.
2. **Parse.** oxc produces an AST. TypeScript syntax is erased or transformed, JSX is transformed. No type checking.
3. **Lower lazily.** The top level becomes bytecode immediately. Function bodies are recorded as source ranges plus a scope skeleton and are not lowered until first call.
4. **Link.** ESM instantiation builds the module graph, allocates module environments, hoists bindings and evaluates in post order. CommonJS `require` resolves at call time. Cycles behave the way Node's do, including the parts nobody likes.
5. **Run in T0.** The interpreter executes bytecode. Each property access site allocates its inline cache on first execution and the opcode quickens itself into a monomorphic variant. Counters accumulate per function and per loop back edge.
6. **Tier up to T1.** Past the call threshold the baseline JIT stitches precompiled stencils into machine code in microseconds. The inline caches carry over unchanged, because they are shared data rather than per tier state.
7. **Tier up to T2.** Past the hot threshold, or on a loop back edge counter overflow, the optimizing JIT builds SSA IR from bytecode plus recorded feedback, speculates, optimizes and emits code with deopt points.
8. **Deopt when wrong.** A failed guard reconstructs an interpreter frame from deopt metadata and resumes in T0. The failed assumption is recorded so the recompile does not repeat it.
9. **Event loop.** After the main script returns, the loop runs timers, pending callbacks, poll, check and close phases, draining microtasks between each, and exits when there is no work left.

Starting thresholds, borrowed from V8's tuned values rather than invented: tier up to T1 at 8 invocations, to T2 at around 500 to 1000 with the counter resetting if feedback shape changes, and a separate loop back edge counter for OSR. All of them are flags so that document 15 can measure the curve instead of us arguing about it.

## 3.4 A program through AOT mode

Steps 1 through 4 are identical, then:

5. **Lower everything reachable.** Reachability comes from the module graph plus statically resolvable dynamic imports and `require` calls. Anything unresolvable keeps the interpreter as its fallback, which is one of the reasons the interpreter ships in the binary.
6. **Feed the specializer.** TypeScript annotations, our own local inference, and optionally a profile from a previous `katsu run --profile`.
7. **Emit Rust.** Each JavaScript function becomes a Rust function containing the operations the optimizing JIT would have emitted, against the same object model, with inlined fast paths where a type is known and a guarded slow path where it is not.
8. **Cargo.** The generated crate depends on `katsu-runtime`, builds with LTO, and produces a static binary containing the program, the runtime, the builtins, the Node layer and the interpreter.
9. **Run.** No parse, no lowering, no resolution, no warmup for anything statically resolved.

## 3.5 The tiers, and why exactly three

| Tier | Compile cost | Relative speed | Memory per function | Triggered by |
|---|---|---|---|---|
| T0 interpreter | zero | 1x | bytecode only | always |
| T1 copy and patch baseline | microseconds | target 5x to 10x | code buffer, evictable | ~8 calls |
| T2 optimizing SSA | milliseconds | target 20x to 50x | code plus deopt metadata | ~500 calls or OSR |

The multipliers are targets shaped by what Deegen and V8 report, not measurements of ours, and document 15 forbids putting them in a README until they are measured here.

Two tiers is not enough: without T1 you either pay optimizing compile time for functions that run twice, or you leave the middle of the distribution running in the interpreter, and that middle is where most server code lives. V8 added Sparkplug and then Maglev for exactly this reason. Four tiers is one more correctness surface than a small team can defend.

The tier boundaries are also the memory policy from document 02.9. Cold functions carry bytecode and nothing else. Warm functions get feedback. Hot functions get code, and that code is evictable under pressure.

## 3.6 Isolates and threads

An isolate owns a heap, its contexts, a bytecode cache and JIT state. It is `Send` but not `Sync`: exactly one thread touches an isolate at a time, and JavaScript objects never cross isolate boundaries except through structured clone or a `SharedArrayBuffer`.

That maps `worker_threads` onto real OS threads with no shared mutable object graph, which means collection is per isolate and never stops the world across threads. It is V8's model and it is the only model that stays sane in a language with `Send` and `Sync` and a moving collector.

It is also the thread per core model from document 02.5: an isolate is pinned to a core, its I/O completions arrive on its own ring, and nothing is work stolen. The consequence for the Rust API is that every JavaScript value is tied to its isolate by a lifetime or a runtime check, which document 11 makes ergonomic rather than annoying.

## 3.7 Why the crate boundaries are where they are

**katsu-ir is the contract between the frontend and every backend.** Bytecode is a versioned, serializable format. That is what makes the bytecode cache, the AOT input and the profile format possible. Changing an opcode bumps the version and invalidates caches.

**katsu-vm does not depend on katsu-jit.** The interpreter runs standalone, which makes `--no-jit` a genuinely supported configuration: for platforms without executable memory, for security sensitive deployments, and most importantly as the reference implementation that document 14's differential fuzzer checks the JIT against.

**katsu-builtins depends only on katsu-vm.** A builtin is written once and works in every tier and in AOT mode.

**katsu-node depends on katsu-api, not the reverse.** The Node compatibility layer is a consumer of the public embedding API. If that API is not good enough to implement `node:fs` on top of, it is not good enough for anyone else either. This is the strongest forcing function available for interop quality and it is deliberate.

**katsu-gc knows nothing about JavaScript.** It sees objects with a header, a scan function and a size. That is what lets document 08 swap MMTk for Whippet on the strength of a measurement.

## 3.8 The realm snapshot

Because startup is axis 1 of the 10x goal, the global object, the prototype chain, every builtin function object, and the interned atom table are built once at compile time and serialized into an image embedded in the binary. Startup maps it copy on write and applies a small relocation fixup.

This constrains the object model in a way worth stating early: nothing in the snapshotted heap may contain an absolute pointer to anything outside it. Compressed slots inside the cage are offsets, so they survive relocation. Native function pointers go through a table indexed by ordinal, resolved at map time. Document 07 carries this as a rule on object layout rather than as an afterthought.

## 3.9 What is deliberately absent

**No AST interpreter.** Everything lowers to bytecode, including `eval`. Two execution semantics is one too many.

**No separate debug and release object layouts.** Assertions compile out, layouts never change. Heisenbugs that only appear in release builds are the worst class of VM bug.

**No plugin system inside the engine.** Extension happens through the Rust API at the embedder level.

**No JIT on the parse path.** Parsing and lowering stay plain safe Rust, so parsing untrusted source has a far smaller trust boundary than executing it.

**No threading of the collector into the builtins.** Builtins allocate through a handle scope and never see a raw pointer, so a moving collector never breaks them.
