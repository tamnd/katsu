# Frontend: parsing, TypeScript, modules, lowering

The frontend turns bytes on disk into bytecode, as fast as possible, doing as little work as possible for code that may never run. Since cold start is axis 1 of the 10x goal, this document is as much a performance document as a correctness one.

## 4.1 We do not write a JavaScript parser

A conformant parser is roughly 30k lines of Rust and close to a person-year once you include automatic semicolon insertion, the cover grammar for arrow functions against parenthesized expressions, regex against division disambiguation, `let` as an identifier, legacy octal in sloppy mode, HTML comments in scripts, tagged template cooked and raw pairs, and then ten years of TypeScript syntax on top.

None of it is differentiating, so we use oxc. It publishes 26.3ms against SWC's 84.1ms and Biome's 130.1ms on typescript.js, passes all test262 stage 4 tests, handles the full TypeScript and JSX grammar, and is maintained as part of VoidZero's toolchain with Rolldown and Vite depending on it.

The cost is an AST we do not control and somebody else's release cadence. The mitigation is that exactly one module consumes the AST, our lowering pass, behind a thin adapter. Switching to SWC later would be a week, not a quarter.

What we do write is the scope analysis, the lowering, and the bytecode. That is where the engine actually lives.

### 4.1.1 The adapter, as built

The mitigation above is only worth anything if it is enforced, so it is. `crates/katsu-parse/src/adapter.rs` is the only file in the workspace that names an oxc type, and `crates/katsu-parse/src/ast.rs` holds the tree everything above it sees. The lowering pass named in 4.1 does not consume the oxc AST after all, it consumes ours, which is a stronger version of the same promise: the surface we depend on is one file and it is small enough to read in a sitting.

Two decisions in the tree are worth writing down because they are cheap now and expensive later.

Every node carries a span from the moment it is built. Source positions retrofitted into a tree are always wrong somewhere, usually in the nodes nobody thought about, and stack traces are a Node compatibility requirement rather than a nicety, so the cost of carrying eight bytes per node is paid up front.

Assignment targets are a separate type from expressions. The grammar allows only a name, a fixed property or a computed property on the left of an equals sign, and encoding that in the type means lowering pattern matches on three cases with no arm for the impossible ones. Every pass that would otherwise have to re-check gets the guarantee for free.

Identifiers are owned strings rather than atoms interned into the heap. `katsu-parse` and `katsu-gc` are both at layer 2 and neither can depend on the other, so interning happens at lowering, which sits above both. That is one allocation per identifier occurrence, and it is a real cost of the layering rather than an oversight. If it ever shows up in a profile the fix is a name table local to the parse, not a layering violation.

Strictness is decided in the adapter and not left for a later pass. A module is strict without saying so, a script is strict only if it opens with the directive, and a function inherits from the code around it and can turn strictness on but never off. The adapter is the only place that still has the nesting in hand while it walks, and strictness changes what `this` is in a plain call and whether an assignment to an undeclared name throws, so it is not a detail that can be filled in afterwards.

The M0 subset is literals, identifiers, `this`, the unary, binary, logical, update, assignment and conditional operators, member access, calls, functions, `var`, `let`, `const`, `if`, `while`, blocks, `return` and expression statements. M1 adds `switch`, `break`, `continue`, object literals, `throw` and `try` with a `catch`. Everything else is refused with the construct named the way a JavaScript programmer would name it, and a line and a column. The refusal list is the M1 and M2 work list read backwards and it should shrink to nothing. The alternative, quietly producing bytecode for syntax the frontend does not understand, is worse than admitting the gap.

A `switch` is worth a paragraph on its own, because three things about it are the opposite of what the syntax suggests and each one is a place an engine can be wrong without any ordinary test noticing. The clauses share one block scope rather than getting one each, so a `let` written in the last clause is in scope for the first and reading it there is a dead zone error rather than a read of the outer name. That scope is instantiated before the first case test runs, not when the first clause runs, so a case test can be in the dead zone of a declaration three clauses further down. And a `default` written in the middle is still compared after every case, so it is a fallback in the comparison order and a position in the layout, which is why we emit the comparisons as a run and then the bodies in source order rather than emitting each clause as a block. Fallthrough falls out of the bottom of one body into the top of the next for free that way, and `break` is a forward jump past the last of them.

`break` and `continue` with nothing to leave are early errors rather than runtime ones, so scope analysis counts the enclosing loops and the enclosing breakables as it walks and refuses in Node's exact words. The count resets at a function boundary, because a loop outside a function is not a loop a `break` inside it can see. A `continue` is lowered as a jump to the loop's back edge rather than to the loop's top, so an iteration that continues still passes through the instruction that counts iterations and a loop that is mostly continues still gets hot.

TypeScript erasure lives here too, and it splits in two. The syntax listed as erasable in 4.2 leaves nothing behind: annotations, interfaces, type aliases, `declare`, overload signatures, and `as`, `satisfies`, type assertions and non null assertions all unwrap to the expression underneath. The syntax listed in the 4.2 transform table is refused by name at M0 rather than dropped, because an enum, a namespace and a parameter property all mean something at run time, and dropping one would leave every reference to it broken in a way that looks like a bug in the runtime rather than a gap in the frontend.

The frontend benchmark measures parsing and adapting together and does not try to separate them. Splitting them would mean making the adapter reachable from outside the crate, which gives up the exact property the module boundary exists to protect, and a program pays for both or neither anyway. Sources are synthetic and every construct in them is inside the M0 subset, so the number is the adapter working rather than the adapter refusing on the first line.

| Source | Size | m4 | gamingpc |
|---|---|---|---|
| 200 small functions | 24.8 KiB | 192 us, 126 MiB/s | 195 us, 124 MiB/s |
| One 600 statement function | 18.1 KiB | 167 us, 106 MiB/s | 231 us, 77 MiB/s |
| 200 functions with TypeScript types | 41.9 KiB | 277 us, 147 MiB/s | 332 us, 123 MiB/s |

The TypeScript row being the fastest per byte is not a surprise once you look at what is in it, since a type annotation is bytes the parser skims and the adapter never allocates for. The one place the two machines genuinely disagree is the long function, where the m4 is 38 percent faster per byte than the i9 while the two are within a couple of percent on the other two shapes. A single 600 statement body is one long vector growing under a deep recursion and nothing else, so that row is closer to a memory subsystem measurement than a frontend one. It is recorded rather than explained away, and it is worth a second look if the gap survives the real lowering pass landing on top of it.

Those three numbers are the parser and the adapter alone, taken before scope analysis existed. `parse` runs the scope pass too now, so the same benchmark ids report a larger total today and the current totals are the ones in 4.4.1. The table is left as it was measured rather than restated, because a number that quietly changes what it counts is worse than a number with a date on it.

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

### 4.2.1 How `katsu check` finds a compiler, as built

Shelling out sounds like one line and it is not, because the interesting part is which compiler gets run. The search is in `crates/katsu/src/check.rs` and it looks in three places in a fixed order.

`KATSU_TSC` wins outright, because somebody who set it has a reason and no amount of searching beats knowing. Then `node_modules/.bin/tsc`, walking up from the file being checked. Then a `tsc` on the path. When there is none anywhere, the error names all three places rather than saying it could not find one, since not found leaves somebody guessing which of the three they were supposed to have set up.

The project compiler beating one on the path is the part worth defending. A project pins its TypeScript version in its `package.json` and its types only check against that version, so a globally installed compiler of a different version is not a substitute for the pinned one, it is a different answer to the same question. Every other tool in this ecosystem resolves this way and a runtime that did not would be the surprising one. On Windows the file to look for is `tsc.cmd`, because npm writes a `.cmd` shim next to the extensionless shell script and Rust will not run the shell script.

What gets checked is a project when there is a `tsconfig.json` at or above the entry, and the single named file when there is not. Checking one file of a project with default settings reports errors the project does not have and misses errors it does, so those are two different commands and the code that picks between them is a pure function with a test on it. Either way `--noEmit` goes on, because this command answers a question and does not produce anything.

The compiler's exit code is forwarded and nothing of ours is printed over the top of it. A summary in front of the diagnostics somebody actually needs to read is noise, and inventing our own exit code would break every script that already branches on `tsc`'s.

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

### 4.4.1 Scope analysis, as built

`crates/katsu-parse/src/scope.rs` runs over the adapted tree and produces three things: a scope per function holding its frame slot count and its cell slot count, a binding per declared name, and a resolution per identifier occurrence. `parse` runs it before returning, so a `ParsedModule` either has an answer for every name in it or does not exist.

Resolutions are keyed by the byte offset the identifier starts at rather than by the order the walk visited it in. A walk order index is a number that stays valid only as long as nobody reorders a pass, and when it does go wrong it silently returns the answer meant for a different variable. A byte offset survives reordering, and a lookup that misses returns nothing instead of returning something plausible and wrong.

The top level is a function scope rather than the global object. Node wraps every CommonJS module in a function before running it and an ES module has its own scope by definition, so a name declared at the top of a file is not a global in either module system, and modelling it as one would mean unpicking that later for both.

Hops count environments and not function boundaries. A function whose bindings are all uncaptured has no environment at run time, so a closure two functions deep can still be zero hops from what it reads. The depth is computed in one forward pass after the walk, `depth[f] = depth[parent] + 1 if f has an environment else depth[parent]`, and a reference costs `depth[reader] - depth[owner]` hops. Getting this wrong is the kind of bug that only appears in a program with an empty function in the middle of a closure chain, which is why it has a test named after exactly that shape.

Capture cannot be decided during the walk, because whether a binding is captured is not known until the whole subtree below its function has been visited. So slot assignment and resolution building are a separate phase that runs after the walk finishes. Uncaptured bindings get frame slots and captured ones get cell slots, which is the analysis 4.4 asks for, and the test that pins it declares twenty names, reads two from a closure, and asserts the environment has two cells.

The early errors are checked here rather than left to the interpreter, because they have to refuse a program even when the line they are on never runs. A redeclaration, a `var` that hoists past a `let` of the same name, a duplicate parameter in strict mode and a `const` with no initialiser are all refused with the message the other engines print, verified against Node 24.18.0 one at a time. Assignment to a `const` is deliberately not in that list: Node reports it as a runtime `TypeError` and not an early error, so the pass exposes the binding kind and lowering emits the throw.

A `catch` parameter is its own kind of binding, and the reason is that it matches none of the three that already existed. It is scoped to the handler like a `let`, so it shadows an outer name and gives it back at the closing brace, and it has no dead zone at all, because the search that finds the handler writes it before the first instruction of the handler runs. It also shares one scope with the handler body rather than sitting in a scope outside it, which is what makes `catch (e) { let e; }` a redeclaration and what makes `catch (e) { var e; }` legal, and the three parts of a `try` are scopes side by side rather than nested, so a `let` in the block does not collide with a `var` in the handler and a `var` in any of the three hoists out of all of them.

The temporal dead zone flag is set when the binding has a dead zone and either the reference crosses a function boundary or it starts before the declarator ends. Comparing against the end of the declarator rather than its start is what makes `let x = x;` throw, which is the case the specification is really about. Crossing a function boundary always sets the flag, because textual order says nothing about when a closure runs. Proving some of those checks dead needs a definite assignment analysis, which is worth doing when there is an interpreter to measure the win against.

| Source | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| 200 small functions | 62.6 us, 26% of the frontend | 79.7 us, 30% | 93.8 us, 25% |
| One 600 statement function | 25.8 us, 14% | 29.4 us, 12% | 30.8 us, 12% |
| 200 functions with TypeScript types | 62.3 us, 19% | 81.9 us, 21% | 84.5 us, 18% |

Measured at one commit on the three reference machines from document 15.5, with the pass timed on its own over a tree that was already adapted, and the percentage taken against the full `parse` on the same source and the same machine. The pass is a fifth to a quarter of the frontend on the function heavy sources and an eighth on the single long body, which is the shape you would expect from a pass whose work is declarations and scope pushes rather than statements.

The TypeScript row costing the same as the plain one is the useful number in the table. That source is 41.9 KiB against 24.8 KiB and it holds the same 200 functions with annotations added, and the pass sees within a percent of the same cost on m4. Erasure happens in the adapter, so by the time scope analysis runs the annotations are gone and there is nothing left of them to resolve. TypeScript is free here in the literal sense.

One allocation was worth removing. The first version copied every declared name twice, once into the binding and once into the map key of the scope it was declared in. Sharing a single `Rc<str>` between the two took 14 percent off the source with 600 declarations and, correctly, nothing measurable off the source with one long function and few names. A change that only helps where the thing it fixes happens is a change you can believe.

Four gaps are recorded rather than hidden. Annex B block level function declarations are not implemented, so a function declared in a block binds in the block and not the enclosing function. `arguments` is detected and flagged on the scope but has no object to resolve to until the interpreter has frames. A captured `let` is always dead zone checked. `eval` and `with` poisoning is written down in 4.4 and is moot in M0, because the adapter refuses both by name.

## 4.5 Bytecode lowering

Register based and three address, specified in document 05. The frontend decisions:

**Registers are assigned by linear scan over the AST**, not by a real allocator. A frame is a contiguous slab of value slots sized at lowering time, and temporaries reuse slots when live ranges do not overlap. We want small frames and fast lowering, not optimal allocation.

**Constants live in a per function pool** and strings are interned isolate wide, so every function using `"length"` shares one entry with the atom table in the realm snapshot.

**Control flow lowers to explicit jumps** with a forward reference patch list. Loop back edges carry a profile counter, which is what triggers OSR.

**Exception handling is a table, not a stack.** Each function carries a table of `(pc_start, pc_end, handler_pc, register)`, ordered so that the first entry containing the throwing instruction is the innermost one. Entering `try` costs nothing, throwing walks the table. Same design as the JVM, and it means `try` in a hot loop is free unless it fires.

**Generators, async functions and async generators are lowered by state machine transformation** at this layer. The body is split at every suspension point, locals live across a suspension move into a heap allocated state object, and resumption is a jump table on the state index. Doing this once in the frontend means neither the interpreter nor either JIT tier needs to know generators exist. It is a meaningful chunk of frontend work and it is worth every line. Async functions are generators driven by a promise loop, async generators are the awkward combination and get their own conformance file.

**Optional chaining, nullish coalescing, logical assignment, destructuring with defaults and rest, spread, class fields, and `super`** all lower onto existing opcodes. Private `#names` get their own opcode family because the brand check has to be fast and must not be a string property lookup.

### 4.5.1 Lowering, as built

`crates/katsu-parse/src/lower.rs` takes the adapted tree and the resolution from 4.4.1 and produces a `FunctionBlueprint` for the top level and one for every function inside it. `parse` runs it before returning, so a `ParsedModule` either holds bytecode that passes `verify` or does not exist. It is a single walk over the tree, with no separate optimisation pass and nothing in between.

Registers are allocated the way a stack machine pushes and pops, except that what comes back is a register number rather than a stack slot. Temporaries start above the frame's named slots, a mark is taken before an expression's operands are evaluated, and the mark is restored once the result is in hand. Frame size is a watermark that allocation raises and nothing lowers, so a function asks for as many registers as its deepest expression needed and not one more. That is the linear scan 4.5 asks for, and it is about thirty lines rather than a pass.

Operands are released before the destination is allocated. That is what keeps `a * b + c` in three registers instead of five, and it is safe for the reason three address code is worth having in the first place: the op has read every operand before it writes anything. A stack machine cannot make that trade because the operands are gone by the time the result exists.

Reading a local returns the variable's own slot instead of copying it into a temporary, which is the whole reason to prefer registers over a stack. It is also the one hazard in the design. If the other operand of a binary expression assigns to that variable, the value read first has to be pinned into a temporary before the write lands on top of it, so `a + (a = 2)` costs a move and `a + b` does not. The predicate that decides is a syntactic walk looking for anything that writes a name, and it is deliberately conservative: pinning a value that did not need it costs one instruction, and failing to pin one that did is a wrong answer.

The calling convention puts argument `n` in register `n`, so registers `0..arity` are reserved even for a parameter that is captured and therefore lives in a cell. The prologue copies those into their cells and the register is then dead. There is no way around that, because the caller has no idea which of the callee's parameters escape and cannot be made to care.

Jump targets are absolute instruction indices patched after the body they jump over has been emitted. Every forward jump is emitted with `CodeOffset(u32::MAX)` rather than zero, so a target nobody ever patched is not a plausible index that happens to be wrong, it is a number `verify` rejects on sight. The highest target ever emitted is tracked too, which is what tells the epilogue whether a function whose last statement is a `return` still needs one more instruction for some jump to land on.

Stores go through a three way enumeration rather than a pair of booleans, because a declaration, an assignment and the write half of `x += 1` check different things: a declaration initialises through the dead zone, an assignment has to check the dead zone and refuse a `const`, and the write half of a compound assignment has already read the variable and so cannot still be in its dead zone. Naming the three cases is what stops the fourth combination, which is meaningless, from being expressible.

A `try` emits no instruction of its own, which is the whole of what the table design buys and is asserted by a test that pins the exact listing of a `try` with a call in it. What lowering emits is the protected block, a jump over the handler, then the handler, and then it pushes one entry recording where the block started, where it ended, where the handler starts and which register the caught value goes in. Two details in that are the search rule rather than bookkeeping. The range is half open and stops before the handler, so a throw inside a `catch` is not caught by the `catch` it is written in. And the entry is pushed after the block it protects has finished being lowered, so a nested `try` reaches the table before the one around it, which is what makes "the first entry that contains the throwing instruction" and "the innermost handler" the same sentence rather than two rules that have to be kept in step.

The register in the entry is the ordinary two shapes a binding has. An uncaptured parameter is a frame slot and the search writes straight into it. A captured one is a cell, and there is nothing that can write into a cell from outside the function, so it lands in a temporary and the first instruction of the handler is a `store_upvalue` that copies it across. A `catch` with no parameter still names a register, because the search has to put the value somewhere and a handler that ignores it is not a handler that can be handed nothing.

`throw` is one opcode and it is a terminator, so nothing is emitted after one and a function whose last statement is a `throw` gets no trailing return. `finally` is refused by name, because it is not a third clause beside the other two: it has to run on the way out of a block that finished, a block that threw and a block that returned or broke or continued, and that is a completion token threaded through every one of those paths rather than another table entry.

Two constructs in the M0 grammar have no bytecode behind them yet. `arguments` needs frames the interpreter does not have, and `new` needs the object model from document 07. Both are refused by name with a source position rather than lowered into something that would have to be unpicked later, and the error reads like every other frontend error because it carries a line and a column.

| Source | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| 200 small functions | 65.0 us, 20% of the frontend | 65.2 us, 20% | 81.7 us, 15% |
| One 600 statement function | 34.6 us, 15% | 32.9 us, 11% | 41.2 us, 13% |
| 200 functions with TypeScript types | 64.9 us, 16% | 64.2 us, 14% | 90.4 us, 17% |

Measured at one commit on the three reference machines from document 15.5, with the pass timed on its own over a tree that was already adapted and resolved, and the percentage taken against the full `parse` on the same source and the same machine. Lowering is a seventh to a fifth of the frontend, which puts it in the same bracket as scope analysis and leaves the parser and the adapter holding roughly two thirds of the budget on every machine. The two passes we wrote are not where the frontend time goes.

The M4 and the pinned Linux core land within three percent of each other on all three sources, which is a pleasant surprise for a laptop against a 13900K and says the pass is bound by memory traffic and branch prediction rather than by clock. The same commit under native Windows is 20 to 40 percent slower on the same silicon. Some of that is the allocator, since `parse/many_functions` is 554 us there against 323 us under WSL2 on the same box, and that gap is far too large to be code generation.

The TypeScript row costs the same as the plain one again, for the same reason it did in 4.4.1: erasure happens in the adapter, so by the time lowering runs there are no annotations left to lower. That is worth restating rather than assuming, because it is the property that makes TypeScript free rather than cheap, and it would stop being true the moment any pass after the adapter had to look at a type.

Scope analysis reads a few percent above the figures recorded in 4.4.1 on two of the three machines and a few percent below on the third. Lowering needs to find the scope belonging to a function it is walking, which is a span keyed map the pass did not build before, and that is the honest explanation for most of it. Removing the fold over every identifier that used to compute the dead zone flags, by setting each flag on the binding during the walk instead, did not measurably pay for it. The change is kept because one pass over the identifiers is better than two, and no speedup is claimed for it anywhere.

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
