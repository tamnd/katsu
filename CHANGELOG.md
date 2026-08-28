# Changelog

Versions are cut on a fixed rhythm rather than when something feels finished. A patch release goes out every few merged pull requests so that there is always a recent tag to bisect against and to point a bug report at, and a minor release, 0.x.0, goes out when a milestone in the roadmap is done. Everything below 1.0 is a skeleton being filled in and nothing here is a stability promise.

## 0.0.5

Two pull requests on, and the thing they add together is that a program can be observed doing something. `console.log` works.

### Globals and functions written in Rust

Globals run, in #38. A name the program did not declare is looked up in a map on the isolate, a name nobody bound is a `ReferenceError` naming the name, `typeof` on a name nobody bound is `undefined` rather than an error, and assigning to a name nobody declared creates it, which is what a sloppy mode script does. The map is a map rather than an object with a shape, and spec 7.4.1 says why that is honest rather than temporary.

A call whose target is a function written in Rust rather than in JavaScript runs too, which is what gives an embedder a way to put something in a realm that a program can call. It holds no code pointer in the cage, only an ordinal into a table the isolate owns, because a function pointer is eight bytes in a four byte world and it points outside the cage entirely.

### Objects and output

An object with properties on it, in #39. A record is a fixed set of names and values with no prototype, no property descriptors, no way to delete a name and no way to add one, because every one of those needs a shape and shapes are M1. That is not what a JavaScript object is and it is exactly what a host object is. The lookup is a linear scan comparing interned addresses, eight compares inside one cache line with no hash and no indirection, and spec 7.4.1 says why that is the right answer at this size rather than a placeholder for a hash table.

Output goes through a sink the isolate owns rather than through `println!`. `Recorder` keeps what a program printed, `Discard` throws it away, and `Standard` is what an isolate nobody has changed has. Replacing one hands back the one that was there, so an embedder can capture output for one call and put the old sink back. Spec 11.4.1 writes it down, since "console.log works" had been a design promise in that document since it was written.

`console` ships with `log`, `error`, `warn`, `info` and `debug` on it. Every argument is inspected and joined with a space, so an object prints its contents rather than `[object Object]`, and a string at the top level prints without quotes while the same string inside an object prints with them. Format specifiers are not written yet and the module doc says so.

`GetProp`, `SetProp` and `CallMethod` run. A missing property is `undefined`, reading a property of `undefined` or `null` throws the message Node throws word for word, and a write that would grow a record is refused with a message naming the reason rather than silently dropped.

### What it costs

Per operation, on the three reference machines from spec 15.5, with the full tables in spec 5.3.5 and 5.3.6.

| Operation | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| Read a global | 2.61 ns | 3.10 ns | 3.44 ns |
| A statement that writes a global | 5.58 ns | 4.81 ns | 5.61 ns |
| Call a function written in Rust | 6.51 ns | 6.29 ns | 9.09 ns |

Reading a global costs about two nanoseconds over a register move, and that is a hash of four bytes rather than of the name, because every name is interned when its unit loads. A function written in Rust is a third cheaper to call than one written in JavaScript on the same call site, which it should be: it pushes no frame and never re-enters the dispatch loop.

The property numbers are in spec 5.3.6 rather than here, because all three boxes were under other work the day they were taken and the durable statement is a ratio rather than a figure. A property read is about three times a global read and about six times a register move, and the three machines agree on both. That is the number M1's inline caches have to beat.

### One bug a benchmark found

The property benchmark's first run reported 43 nanoseconds for a read, more than three times what a scan of eight addresses could cost. The cause was not the lookup. The opcode arm was building the text of the property name for the error message it might need, on the way through, whether or not it was going to fail, and reading a constant back out of the pool allocates a `String`. Moving the message into a cold function that a successful read never calls took the same benchmark from 43.0 to 11.8 nanoseconds on the same machine minutes apart. Spec 5.3.2 has three of these written down now and this is the fourth: the cost was in code that was there for the case that does not happen.

### Also

A benchmark filter reaching a reference machine was being pasted into a remote shell line unquoted, so any filter containing a regex alternation ran as a pipeline and failed. It is quoted per shell now, single quotes on the bash side and a batch file on the Windows side, and a filter containing a double quote is refused rather than mangled. Every filtered remote run had been silently broken.

## 0.0.4

Four pull requests on. Bytecode executes now. A source file goes from text to a value as long as the program stays inside what M0 has, which is numbers, strings, booleans, locals, control flow, functions and closures. Objects, globals and property access are the next thing, and they are what stands between this and `console.log`.

### The stack

The interpreter got its own stack, in #33. Eight megabytes of address space reserved at startup and sixty four kilobytes committed, growing a chunk at a time and never shrinking, because a program that recursed once usually recurses again and the syscall to hand a page back costs more than the page. The depth limit is ten thousand frames, which is roughly where Node raises `RangeError`, and hitting it is an ordinary error a program could catch rather than a crash.

The frame header is not inline in the region, which is a deliberate deviation from the drawing in spec 5.4. Values live in the region and headers live in a vector beside it, so the root set is a slice with nothing to skip and nothing to get wrong. The cost is a second allocation and a second cache line per call, which is measured in the benchmarks rather than asserted.

### Dispatch

The `loop { match }` dispatch loop, in #34. Every opcode is one arm, the arm does the work, and nothing hides behind a generic helper that takes a closure. Arithmetic, the bitwise operators, comparisons, the unary operators, the temporal dead zone checks, jumps, back edges and `return` all run. The numeric conversions live in their own module because each one is a place where the obvious Rust expression is subtly not what JavaScript says: `ToInt32` is a modulo and a fold rather than a cast, a shift count is taken modulo thirty two, and exponentiation disagrees with IEEE `pow` in exactly two places.

Back edges check one shared atomic word, so an endless loop can be stopped from another thread, and a test proves it.

### Strings

Strings in the interpreter, in #35. A literal reaches a register, two of them concatenate, they compare by code unit, and they convert to numbers and booleans by the rules the standard gives. The interpreter owns an isolate to allocate them in.

Three things about that work were worth writing down in spec 5.3.2, because none of them was in the arm being measured and all three will come back. A conversion inlined into the switch dragged a string decoder into the dispatch loop and made arithmetic 167 percent slower. A heap path marked out of line but not cold cost 32 percent of the counting loop on Windows while Linux on the same silicon was flat. And holding the isolate inline rather than behind a `Box` made the interpreter three hundred and forty four bytes instead of seventy two, which a register move could feel.

### Calls, closures and environments

Calls run, in #36. A call pushes a frame, copies its arguments out of a run of the caller's own registers, runs the callee and writes what it returned into the register the call names. A function written inside another one closes over the environment it was written in, and a captured variable moves out of a register into a context, which is a heap object holding one cell per captured variable and a pointer to the context outside it.

The heap has three kinds of object where it had one. A closure and a context join the string, and they are told apart by the word every object already starts with, the one that holds a shape from M1 onward. That word is a slot, so a shape is a pointer and a kind tag is a small integer, and a tag written there today can never be mistaken for a shape. Zero is a string, which costs nothing to record because pages come back zeroed and the string allocator never wrote that word.

### What it costs

Per call, on the three reference machines from spec 15.5, with the full tables in spec 5.3.4.

| Operation, per call | m4 | gamingpc | gamingpc-win |
|---|---|---|---|
| A call and a return | 7.01 ns | 9.46 ns | 9.90 ns |
| The same through a closure that reads one captured variable | 14.71 ns | 15.49 ns | 15.87 ns |
| A call inside `fib(20)` | 20.42 ns | 23.24 ns | 23.50 ns |

The first Node comparison this project can make honestly, on the same `fib(20)`, the same machines and the same pinning. Node with `--jitless` is V8's interpreter with the optimizing tiers turned off, which is the fair comparison for what we have today.

| `fib(20)`, per call | m4 | gamingpc-win |
|---|---|---|
| katsu, interpreter only | 20.42 ns | 23.50 ns |
| Node 26 with `--jitless` | 14.13 ns | 17.78 ns |
| Node 26 as it ships | 1.62 ns | 1.60 ns |

Our tier 0 is within about a third to a half of V8's tier 0. Node as it ships is twelve to fifteen times faster than either interpreter, because TurboFan compiled `fib` and unboxed its arithmetic. Nothing in M0 closes that gap and nothing in M0 claims to.

### Known gaps

Pushing and popping a frame got 24 percent slower on the pinned Linux machine, from 6.40 ns to 7.99 ns, when the frame header grew from sixteen bytes to twenty four to hold the function index and the context. That is the whole cause, isolated by applying only those two fields to the previous commit, and the route back down is written into spec 5.4.1 as a trade rather than a win.

A context cell is eight bytes where everything else in the cage is four, because a captured variable can be a double or `undefined` and there is no heap number and no realm singleton to point at yet. It goes back to four in M1.

`Stack::roots` is no longer the whole root set, because a frame's context is a heap pointer that lives in the header rather than in a register. Nothing collects yet, so this is a note for M1 rather than a bug.

A function joined to a string prints as `[Function: name]` where Node prints the source text, because carrying source spans on a function is a later piece of work.

The gaps from 0.0.3 are unchanged: no ropes so concatenation is quadratic, no hash flooding resistance until the realm can carry a per process seed, the atom table's buckets sit outside the cage and miss the heap census, the four in the scope pass, `arguments` and `new` refused by name, and the native Windows frontend running 20 to 40 percent slower than WSL2 on the same silicon.

## 0.0.3

Three pull requests on. The frontend section of M0 is finished, so a source file now goes all the way to verified bytecode. Nothing executes that bytecode yet, and the interpreter is the next thing.

### Bytecode

An instruction set to lower into, in #29. Sixty odd opcodes in decoded enum form, register based and three address, with the byte encoding deliberately deferred until there is an on disk cache that needs one. A `FunctionBlueprint` carries the code, the constant pool, the source positions and the frame size, and it can verify itself: every register inside the frame, every jump target inside the code, every constant index in range, and the last instruction a terminator. The disassembler exists for the same reason, so a lowering test asserts on something that reads like bytecode rather than on a struct literal.

Source positions are stored as a delta compressed sidecar rather than a field on each instruction, so an instruction stays small and a position is still available for every one of them. Retrofitting positions after the fact is a thing that never actually happens, so they are there from the first opcode.

### Frontend

Lowering, in #31. A single walk from the resolved tree to a blueprint per function, with registers allocated on a stack discipline and the frame size as a watermark that allocation raises and nothing lowers. Operands are released before the destination is allocated, which keeps `a * b + c` in three registers instead of five and is safe because a three address op reads every operand before it writes anything. Reading a local returns the variable's own slot instead of copying it, and the one hazard that comes with that, an operand that assigns to the variable the other operand just read, is handled by pinning the earlier value into a temporary.

Jump targets are absolute instruction indices patched after the fact, emitted as `u32::MAX` rather than zero so that a target nobody patched is a number the verifier rejects on sight instead of a plausible index that happens to be wrong.

Lowering is a seventh to a fifth of the frontend, measured on all three reference machines and recorded in spec 04.5.1. Together with scope analysis that is about a third of the frontend, which puts the other two thirds in the parser and the adapter, and it says the startup budget will be won in the laziness work rather than in either pass we wrote. TypeScript annotations cost lowering nothing, for the same reason they cost scope analysis nothing: erasure happens in the adapter.

### Packaging

The Intel macOS binary is built on a runner that still exists, in #30. GitHub retired the Intel macOS image and the release job had been pointing at it.

### Known gaps

The three from 0.0.1 are unchanged: no ropes so concatenation is quadratic, no hash flooding resistance until the realm can carry a per process seed, and the atom table's buckets sit outside the cage and so miss the heap census. The four from 0.0.2 in the scope pass are unchanged too. Two more are named in lowering: `arguments` and `new` are refused by name with a line and a column, because the first needs frames that do not exist and the second needs the object model from spec 07.

Native Windows runs the frontend 20 to 40 percent slower than WSL2 on the same silicon, and `parse` on a file of small functions is 554 us against 323 us. That gap is too large to be code generation and it is written down here so that it gets chased rather than absorbed.

## 0.0.2

Two pull requests on from the first tag. Still nothing runs, and the interpreter is still the next thing, but the platform layer now covers all three operating systems and the frontend resolves every name it parses.

Neither this tag nor 0.0.1 has binaries attached to it. The release workflow asked for an Intel macOS runner by a label GitHub retired on 4 December 2025, and a job that asks for a runner that does not exist neither fails nor times out, it queues until GitHub gives up a day later, so the publish step that needed it never ran and nothing said so. Fixed in #30. Moving either tag forward onto the fix would make the tag contain work its own entry does not describe, so both are left where they are and 0.0.3 is the first tag that publishes anything. Build from source at either tag and you get exactly what the entry says.

### Platform

Windows is supported and in the test matrix on every commit, in #26. The virtual memory seam is five items wide, `page_size`, `reserve`, `release`, `commit` and `decommit`, with one file per platform picked by `cfg` at the module boundary, so nothing above the seam knows which one it got.

Getting it running on Windows found a real bug that was invisible on Linux and macOS. Growing the heap was recommitting the whole range from the base each time rather than only the new pages, which is quadratic, and `mprotect` on an already permitted range is cheap enough that it never showed up. `VirtualAlloc` with `MEM_COMMIT` walks every page whether or not it is already committed, so the same code on Windows made the cost impossible to miss. Committing pages still costs about four times as much on Windows as on Linux on the same silicon, which is measured and written into spec 07.

### Frontend

Scope analysis, in #27. Every identifier in a parsed module resolves to a local slot, an upvalue at a known depth, or a global, and a `ParsedModule` either has an answer for every name in it or does not exist. Uncaptured bindings live in frame slots and never touch the heap, captured ones get cells, and hops count environments rather than function boundaries, so a closure two functions deep can still be zero hops from what it reads.

The early errors are checked here rather than left to the interpreter, because they have to refuse a program even when the line they are on never runs. A redeclaration, a `var` that hoists past a `let` of the same name, a duplicate parameter in strict mode and a `const` with no initialiser are refused with the message Node prints, checked against Node 24.18.0 one at a time. Assignment to a `const` is not among them, because Node reports that as a runtime `TypeError`.

The pass is a fifth to a quarter of the frontend on function heavy sources and an eighth on a single long function body, measured on all three reference machines and recorded in spec 04. TypeScript annotations cost it nothing, because erasure happens in the adapter and there is nothing left of them by the time it runs.

### Known gaps

The three from 0.0.1 are unchanged: no ropes so concatenation is quadratic, no hash flooding resistance until the realm can carry a per process seed, and the atom table's buckets sit outside the cage and so miss the heap census. Four more are named in the scope pass: Annex B block level function declarations, `arguments` being flagged but having no object to resolve to, a captured `let` always being dead zone checked until there is a definite assignment analysis, and `eval` and `with` poisoning, which is moot while the adapter refuses both by name.

## 0.0.1

The first tag. Four pull requests into M0, and what exists is the bottom of the value and object model plus the front of the frontend. Nothing runs yet: there is no interpreter and `katsu run` does not do what its help text says.

### Values and memory

The tagged value representation is decided and written down, JSC style NaN boxing with a 2^49 offset on the double encoding, in #21.

The heap is a 4 GiB pointer compression cage aligned so that decompressing a slot is a bitwise or and compressing one is a truncation, with a 4 GiB guard region above it and bump allocation inside it, in #22. A slot is 32 bits with the tag in bit zero, which makes a slot of all zeroes the integer zero, so a freshly zeroed page needs no initialisation. Reserved memory and committed memory are kept rigorously apart, because a container's memory limit counts pages and not liveness.

Flat strings and the atom table, in #23. A twelve byte header rather than the sixteen the budget assumed, by packing the hash and the flags into one word. Latin-1 and UTF-16 with a canonical representation, so a string is only wide if it actually holds a code unit above 255 and equality can reject on the header alone. The hash is defined over code units rather than stored bytes specifically so that an atom lookup can hash a Rust string slice without allocating a candidate first, and there is a test asserting that a lookup which misses leaves the heap cursor where it was.

### Frontend

The oxc syntax tree is adapted into one of our own, in #24. One file names an oxc type and nothing above it does, which is what makes the parser swappable. Spans are carried from the moment a node is built, assignment targets are their own type, and strictness is resolved during the walk. TypeScript that erases is erased and TypeScript that emits code is refused by name, with a line and a column, along with everything else outside the M0 subset.

### Known gaps

There are three named in code and spec rather than hidden. String concatenation is quadratic because there are no ropes yet, which is M1 work. There is no hash flooding resistance, because the per process random seed has to arrive with the realm and the realm does not exist yet. The atom table's buckets are allocated outside the cage and so do not show up in the heap census.
