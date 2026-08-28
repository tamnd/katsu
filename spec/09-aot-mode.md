# AOT mode: compiling to Rust

## 9.1 What the command does

```
katsu build ./src/main.ts -o ./bin/app
```

The frontend from document 04 parses and lowers the whole reachable module graph. The AOT compiler then emits a Rust crate: one module per JavaScript module, one function per JavaScript function, plus the constant data and the realm initializer. Cargo builds it and links it against the katsu runtime library. The result is a single native executable that starts in about a millisecond and needs nothing installed.

The generated crate is a real artifact on disk, not a hidden temporary. `katsu build --emit-rust ./out` writes it and leaves it there, because a compiler whose output you can read is one you can debug, and because it is the honest way to show a skeptical user what the tool is actually doing.

## 9.2 Why Rust is the target language

The alternative was emitting machine code directly, reusing the tier 2 backend from document 06.

Emitting Rust wins on four counts. We inherit LLVM's optimizer, which is a decade of loop and vectorization work we are not going to reproduce. We inherit cross compilation, so `--target aarch64-unknown-linux-gnu` is free. Rust interop stops being a bridge and becomes ordinary function calls in the same crate graph, which is the point of document 11. And the generated code is inspectable and auditable in a way that a machine code blob is not.

It loses on compile time, which is the real cost and is addressed in 9.8.

Static Hermes made the same structural choice with C rather than Rust, and Perry made it with LLVM IR directly through SWC. The Rust choice is what makes the two way interop story work, and interop is half of what this project is for.

## 9.3 The uncomfortable truth about AOT and JavaScript

A JIT's advantage is not that it generates better instructions. It is that it knows what actually happened. An AOT compiler looking at `function f(a, b) { return a + b }` has to emit code that handles two numbers, two strings, an object with `valueOf`, a Symbol that throws, and a Proxy that runs arbitrary code, because all of those are legal.

The best published measurement of how far you can get without runtime code generation is Chris Fallin's work on compiling JavaScript to WebAssembly ahead of time. Rather than generating stubs at runtime, it pre-compiles a corpus of commonly observed CacheIR patterns and dispatches to them indirectly, then uses profile guided optimization to inline hot IC targets and turn indirect calls into conditional branches. The result on Octane was a 2.77x geometric mean speedup over the interpreter, ranging from 0.90x to 4.39x, against roughly 5x for native SpiderMonkey baseline compilation.

Read that carefully, because it sets our expectations. Pure AOT on untyped JavaScript gets you a bit over half of what a baseline JIT gets, and a baseline JIT is itself well short of an optimizing one. **AOT mode does not beat JIT mode on untyped dynamic code and this document will not pretend otherwise.**

What AOT does win, unconditionally, is startup, binary size, memory, predictability, and deployability. Those are four of the seven axes in document 02, and they are the four we claim 10x on.

And there is one more thing AOT wins, which is the whole reason the mode exists.

## 9.4 What types buy

When the compiler can prove that a value is a number, the generated code is `a + b` on two `f64` registers with no check, no box, no cache slot and no branch. That is not 20% faster than the dynamic version, it is one instruction against a dozen. This is the effect behind Static Hermes's widely quoted 300x microbenchmark number, and behind Perry reporting integer recursion within a couple of percent of Rust at `-O`.

So the AOT compiler runs a type inference pass over the lowered IR, and every value gets one of:

| Level | Meaning | Code emitted |
|---|---|---|
| Proven | inference established the type without any assumption | unboxed native operation, no guard |
| Speculated | annotations or a profile say so, but it is not proven | unboxed operation behind a guard, deopt on failure |
| Dynamic | genuinely polymorphic | tagged values, inline cache, the same path the interpreter takes |

The inference is a local flow analysis plus interprocedural propagation for functions that are not reassigned, seeded by literals, arithmetic results, `typeof` guards, and TypeScript annotations.

**TypeScript annotations are evidence, not proof.** This is the rule that separates us from Static Hermes and Perry, and it follows directly from the project goal of running 100% of existing programs unmodified. TypeScript's type system is deliberately unsound: `any`, `as` casts, non-null assertions, `JSON.parse` returning `any`, declaration files that lie, and structural types that are erased at runtime all mean an annotation can be wrong in a program that type checks. A compiler that trusts `x: number` and emits an unchecked `f64` load will produce silent memory corruption on the day someone passes a string.

So an annotation raises a value to Speculated, never to Proven. The guard is real and the deopt path is real. On correct programs the guard is one predictable compare that costs a fraction of a percent, and on incorrect programs we are still a correct JavaScript engine.

`"use static"` at the top of a file is the opt in for users who want the other trade: within that file annotations are trusted, guards are dropped, and violations are undefined behavior in the same sense they are in AssemblyScript. It is off by default, it is documented as a sharp tool, and it exists because some users genuinely do want the last 20%.

## 9.5 Deoptimization without a JIT

A guard that fails in an AOT binary has nowhere to tier down to, unless we put somewhere there. So we do: **the interpreter is linked into every AOT binary**, along with the bytecode for every function that has any speculated code in it.

A failed guard reconstructs an interpreter frame using the same frame state machinery as document 06.5, and execution continues in the interpreter from the recorded bytecode offset. The function is marked so that subsequent calls go straight to the interpreter rather than re-entering code whose assumption is known to be false.

This costs binary size, which is why document 15 measures it, and it is the price of being a correct JavaScript runtime rather than a compiler for a JavaScript-shaped language. Functions where every value is Proven carry no bytecode and no deopt metadata at all, so a fully typed numeric core compiles to pure native code with nothing behind it.

## 9.6 Profile guided AOT

Static types cover the numeric parts of a program. Profiles cover the rest, and this is where the Fallin result says most of the remaining win lives.

```
katsu run --profile=app.prof ./src/main.ts      # run a representative workload
katsu build --profile=app.prof ./src/main.ts    # build using what it learned
```

The profile is the feedback vector contents from document 05.5, serialized: which shapes each property site saw, which functions each call site reached, which arithmetic sites stayed int32, which branches went which way, and which functions ran hot at all.

The AOT compiler consumes it exactly the way tier 2 consumes feedback. Monomorphic property sites become a shape guard and a fixed offset load. Call sites with one observed target get inlined. Cold functions are compiled for size rather than speed, or left as bytecode entirely.

Without a profile, we fall back to pre-compiled IC stubs dispatched indirectly, which is the Fallin baseline. With a profile, we approach what a warmed up baseline JIT produces, because at that point we have the same information it does.

The profile is a build input, so it is versioned, diffable, checkable into a repository, and it never changes semantics. A stale profile makes the binary slower, never wrong. That property is non negotiable and it is a test.

## 9.7 The dynamic residue

Some constructs cannot be compiled ahead of time and have to be honest about it.

**`eval` and `new Function`** need the parser and the lowering pass in the binary. They are included by default, which costs roughly the size of oxc plus our frontend. `--no-dynamic-code` drops them, makes `eval` throw, and is the right default for serverless and embedded targets. This mirrors the Content Security Policy situation on the web and is a documented product flag, not a limitation to be discovered.

**Dynamic `import()`** of a statically analyzable specifier is resolved at build time and included. Of a computed specifier, it either resolves against a bundled module map or falls back to loading from disk at runtime, and `--static-only` turns the unresolvable case into a build error rather than a runtime surprise.

**`Function.prototype.toString`** must return the original source, so source text for functions that could observably be stringified is retained. Source retention is a size line item and there is a flag for programs that do not care.

**Monkey patching builtins** is legal and common, so the realm is a real mutable realm, not a set of hardcoded calls. `Array.prototype.map` is a call through the object model unless inlining proved the prototype unmodified, with a validity cell backing that proof exactly as in document 07.5.

## 9.8 Build times, which are the thing users will complain about

`cargo build --release` on a large generated crate is slow, and the developer loop is where a compiler earns or loses its users.

The plan is layered. `katsu run` never invokes AOT at all, so the edit and test loop is the JIT, which starts in milliseconds. `katsu build --dev` uses the Cranelift codegen backend and `-C opt-level=1` for a fast build with reasonable output. `katsu build` is the release path and is expected to take real time on a large program. Generated modules map one to one onto JavaScript modules, so incremental rebuilds recompile only what changed, and the emitted crate is deterministic so that `sccache` and Cargo's own caching work.

We also aggressively do not emit what we do not need. A program importing only `node:fs` and `node:path` does not link the HTTP stack. This is ordinary dead code elimination made possible by the module graph being static, and it is what keeps binaries near Perry's reported 330 KB hello world and single digit megabytes for real applications rather than near Node's 110 MB.

## 9.9 Where AOT mode is the right answer

Command line tools, where a 1ms start against Node's 25 to 40ms is the entire user experience. Serverless functions, where cold start is billed. Containers, where a 15 MB image against a 160 MB one changes the deployment. Embedded and edge targets with no room for a JIT and often no writable executable memory at all. Desktop and mobile applications shipping a binary. Numeric and data processing code with real types, where the typed path is the whole point.

Long running servers are the case where the JIT wins, because a tier 2 compiled hot path with real feedback beats anything static. Document 13 makes both modes first class rather than treating AOT as a lesser sibling, and document 15 publishes the comparison honestly in both directions.
