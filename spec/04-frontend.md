# Frontend: parsing, TypeScript, modules, lowering

The frontend turns bytes on disk into bytecode, as fast as possible, doing as little work as possible for code that may never run. Since cold start is axis 1 of the 10x goal, this document is as much a performance document as a correctness one.

## 4.1 We do not write a JavaScript parser

A conformant parser is roughly 30k lines of Rust and close to a person-year once you include automatic semicolon insertion, the cover grammar for arrow functions against parenthesized expressions, regex against division disambiguation, `let` as an identifier, legacy octal in sloppy mode, HTML comments in scripts, tagged template cooked and raw pairs, and then ten years of TypeScript syntax on top.

None of it is differentiating, so we use oxc. It publishes 26.3ms against SWC's 84.1ms and Biome's 130.1ms on typescript.js, passes all test262 stage 4 tests, handles the full TypeScript and JSX grammar, and is maintained as part of VoidZero's toolchain with Rolldown and Vite depending on it.

The cost is an AST we do not control and somebody else's release cadence. The mitigation is that exactly one module consumes the AST, our lowering pass, behind a thin adapter. Switching to SWC later would be a week, not a quarter.

What we do write is the scope analysis, the lowering, and the bytecode. That is where the engine actually lives.

## 4.2 TypeScript: erase, transform, never check

`katsu run` treats TypeScript as JavaScript with extra syntax. No type checking at runtime, ever. Node, Deno and Bun all behave this way and users expect it.

Erasable syntax is dropped: annotations, interfaces, type aliases, generics, `as` and `satisfies`, `declare`, non-null assertions, import and export type specifiers, abstract members, overload signatures.

Non erasable syntax is transformed rather than rejected:

| Syntax | What we emit |
|---|---|
| `enum` | the object plus its reverse mapping initializer |
| `const enum` | inlined values, with the object retained when `preserveConstEnums` semantics apply |
| `namespace` with runtime members | the IIFE and merged object |
| parameter properties | the constructor assignments |
| decorators | the stage 3 standard semantics, with legacy `experimentalDecorators` behind a flag |
| `import x = require()` | a `require` call |
| JSX | `React.createElement` or the automatic runtime, config driven |

Node's `--experimental-strip-types` historically refused several of these. We accept all of them, because a large share of existing TypeScript uses enums and namespaces, and refusing them makes us look broken in the first five minutes of somebody's evaluation.

`katsu check` shells out to TypeScript 7, which shipped July 2026 as a Go native compiler that type checks the VS Code codebase in 10.6 seconds against 125.7 on the old one. We do not ship a type checker and we do not pretend to. Note for document 09: TypeScript 7.0 has no stable programmatic API, that is promised for 7.1, so anything that wants machine readable checker output has to wait or parse JSON diagnostics.

## 4.3 Module resolution

This has to be a bug for bug reimplementation, because getting it 95% right means five percent of packages fail to load and the user has no idea why.

The pieces: CommonJS lookup walking `node_modules` up the tree with extension probing and directory index files; the ESM `ESM_RESOLVE` algorithm with URL semantics and mandatory extensions; `"exports"` and `"imports"` with conditional exports, nested conditions and pattern trailers; the condition list including `node`, `import`, `require`, `default` and our own `katsu` condition that packages can opt into; `"type": "module"` determination with `.mjs` and `.cjs` overrides; self referencing a package by name; `node:` prefixed builtins and the unprefixed names Node still allows; import attributes for JSON.

Import maps are a Deno concept and are not supported.

The interop rules are the part that actually breaks people, and they get a test matrix rather than prose: `require()` of an ESM graph and the conditions under which Node permits it, `import` of CommonJS producing a default export plus the named exports a lexer can detect, `__esModule` interop, and live bindings against snapshot copies.

We evaluate `oxc-resolver` before writing our own. It is MIT licensed, actively maintained as of 26 August 2026, and claims 28x faster than enhanced-resolve. If it is exact, we depend on it. If it is not, we write ours and keep it behind the same trait.

**The startup consequence.** Resolution is filesystem syscalls, and it is a large share of Node's cold start. We cache the resolved graph keyed by the content hashes of every `package.json` that participated plus directory mtimes, so a warm run does almost no probing. That is how we get most of LLRT's startup win without demanding that users bundle their application into one file.

## 4.4 Scope analysis

Every identifier reference is resolved statically at lowering time into one of:

- a local slot in the current frame, which is the common case and costs one register access
- an upvalue slot at a known depth up the environment chain
- a module binding with an index, for ESM live bindings
- a global, which becomes a property access on the global object through an inline cache
- dynamic, only inside `with` or a function containing direct `eval`

Variables never captured by a closure live in registers and never touch the heap. Captured variables get a cell and the environment holds the cell. We run the standard analysis: if nothing in a function's subtree references a binding, it gets no cell. In real code most closures capture two variables out of twenty, so this one analysis is worth a lot of allocation.

`var` hoisting, TDZ for `let` and `const`, function declaration hoisting including the Annex B sloppy mode block level rules, `arguments` materialization only when `arguments` is actually referenced, and the `this` binding rules are all settled here so that the interpreter never has to ask.

Direct `eval` and `with` poison their enclosing function's scope, which loses static resolution for all its bindings. That is what every engine does, and it means those two features cost the program that uses them instead of costing everyone.

## 4.5 Bytecode lowering

Register based and three address, specified in document 05. The frontend decisions:

**Registers are assigned by linear scan over the AST**, not by a real allocator. A frame is a contiguous slab of value slots sized at lowering time, and temporaries reuse slots when live ranges do not overlap. We want small frames and fast lowering, not optimal allocation.

**Constants live in a per function pool** and strings are interned isolate wide, so every function using `"length"` shares one entry with the atom table in the realm snapshot.

**Control flow lowers to explicit jumps** with a forward reference patch list. Loop back edges carry a profile counter, which is what triggers OSR.

**Exception handling is a table, not a stack.** Each function carries a sorted table of `(pc_start, pc_end, handler_pc, context_depth)`. Entering `try` costs nothing, throwing walks the table. Same design as the JVM, and it means `try` in a hot loop is free unless it fires.

**Generators, async functions and async generators are lowered by state machine transformation** at this layer. The body is split at every suspension point, locals live across a suspension move into a heap allocated state object, and resumption is a jump table on the state index. Doing this once in the frontend means neither the interpreter nor either JIT tier needs to know generators exist. It is a meaningful chunk of frontend work and it is worth every line. Async functions are generators driven by a promise loop, async generators are the awkward combination and get their own conformance file.

**Optional chaining, nullish coalescing, logical assignment, destructuring with defaults and rest, spread, class fields, and `super`** all lower onto existing opcodes. Private `#names` get their own opcode family because the brand check has to be fast and must not be a string property lookup.

## 4.6 Laziness is the startup story

Parsing a 5 MB bundle eagerly and lowering every function costs hundreds of milliseconds, and most of it is wasted because a typical program calls a small fraction of the functions in its dependency tree.

So we pre-parse and then lower on first call. The initial pass over a function body does the minimum needed to know where it ends, whether it is strict, which outer bindings it captures, and whether it contains `eval`. Full lowering happens at first call.

There is a real question of whether oxc is fast enough that a separate pre-parse pass is a pessimization, and we should instead parse fully but lower lazily, keeping the AST arena alive. That is a memory against time trade and document 15 measures it with both implementations behind a flag rather than us guessing.

**Bytecode caching.** After lowering, a module's bytecode is serialized to a cache keyed by content hash plus bytecode version. The second run skips parse and lowering entirely. It is the same format AOT mode reads, which is why it is specified rather than being whatever `bincode` produced that day. Invalidation is by hash, never by mtime.

## 4.7 What the frontend hands over

```
FunctionBlueprint {
  bytecode      Vec<u8>            versioned and serializable
  constants     Vec<Constant>
  registers     u16                frame size
  params        u16
  flags         strict, arrow, generator, async, method, ctor kind, uses_arguments
  upvalues      Vec<UpvalueDesc>
  exceptions    Vec<HandlerEntry>
  source_range  (u32, u32)         for Function.prototype.toString and stack traces
  line_table    compressed pc to line and column
  ic_slots      u16                how many inline caches this function needs
  feedback      FeedbackVectorDesc allocated lazily, see document 07
}
```

Deciding `ic_slots` and the feedback descriptor at lowering time rather than discovering them at runtime is what lets the interpreter's caches be a flat array indexed by an operand instead of a hash lookup. That is a measurable interpreter win and it costs one counter in the lowering pass.

The feedback vector itself is not allocated here. Under the memory budget in document 02.3 it is allocated on first execution of the first site that needs it, so a function that is defined and never called costs bytecode and nothing else.
