# Changelog

Versions are cut on a fixed rhythm rather than when something feels finished. A patch release goes out every few merged pull requests so that there is always a recent tag to bisect against and to point a bug report at, and a minor release, 0.x.0, goes out when a milestone in the roadmap is done. Everything below 1.0 is a skeleton being filled in and nothing here is a stability promise.

## 0.1.8

0.1.7 did not finish, so this is 0.1.7 again with the part that failed fixed. Nothing in the engine changed between the two and every number in the entry below still stands.

What failed is worth writing down rather than quietly correcting. The first release to publish the crates uploaded `katsu-ir` and then stopped with a 429, because crates.io lets one account create a burst of five new crate names and then one every ten minutes. Fifteen new names in one `cargo publish --workspace` was never going to work, and there is no flag anywhere in cargo for skipping a version that is already uploaded, so a re-run of that job would have failed immediately on the crate that did go up. The rate limit was researched before the workflow was written and this specific limit was not found, which is the honest version of what went wrong.

`cargo xtask publish` replaces the workspace publish in the release workflow. It works the order out from the dependency graph the same way, asks crates.io per crate whether the version is there and skips it if it is, and when it does hit the limit it reads the time out of the message and waits until then. A stopped release now finishes on a re-run and a first release finishes on its own, at the cost of a job that sits still for a little over two hours the one time fifteen names are new. Every release after this one is fast, because the limit for a new version of a crate that already exists is a burst of thirty. CI runs the same command with `--dry-run` on every commit, and `cargo publish --workspace --dry-run` stays where it was, because verifying that the whole set builds as if it were already published is the one thing only that command does.

`katsu-ir 0.1.7` stays on crates.io as the only piece of that tag. Yanking it would say the version is broken and it is not, it is simply alone, and the answer to a partial release is another release.

The 0.1.8 upload got no further, and the reason is a second thing worth writing down. Waiting until the moment the rate limit message names is not enough, because the bucket charges a token for a request it refuses as well as for one it accepts: asking the second the deadline passes takes the refilled slot away from the attempt that would have used it, and the next deadline moves another ten minutes out. Three runs did that, one deadline at a time, and none of them got past the second crate. A publish now paces itself between new crate names instead of reacting to refusals, waits a whole refill period past the second deadline when it is refused twice, and shares one concurrency lane so two runs cannot spend the same allowance on each other. That fix is on main rather than in this tag, because a tag cannot be changed and a re-run of the release job runs the code as it was at the tag, so a manual `Publish crates` workflow runs the same publish from the default branch and finishing 0.1.8 goes through it. It ships as code in 0.1.9.

The workload baseline for this release is measured against the 0.1.8 tarball rather than a local build, so it lands after the tag and is quoted in the pull request that follows this one, the same way 0.1.6 was done.

## 0.1.7

The seventh patch release of M1, four pull requests on from 0.1.6, and the one sentence that matters is that the wall came down. `throw new Test262Error(message)` is the first line of most of test262, `new` did not exist for six releases, and the conformance number sat at 6.66 percent for five of them because of it. It is 12.31 percent now, 9,995 cases against 5,410, and nothing regressed on the way.

### `new` and `instanceof`

In #77. `new Foo()` builds an object whose prototype is `Foo.prototype`, runs the constructor against it, and gives back the constructor's object if it returned one or the fresh object if it did not. `x instanceof Foo` walks the chain and answers. They are one piece of work because they are the same three steps in a different order, and both of them start with a property read on a function, which is what 0.1.6 was for.

Constructing is two opcodes rather than one. The fresh object has to still be somewhere when the call comes back, so `Construct` parks it in the register the callee was read into, which is dead the moment the frame is pushed, and `ConstructResult` picks between that register and whatever came back. The alternative was a word on the call frame saying this frame is a construct, read by every return in the program to serve the calls that are constructs. The frame is thirty two bytes because the last eight cost 14 percent on `call/call_return`, so the same decision was made the same way: one dispatch per `new`, next to an allocation that costs far more, rather than a load per return.

Three real bugs came out of testing rather than out of reading. `new Foo() instanceof Bar` threw about `Bar`'s prototype, because the lookup went through the path that deliberately does not build a function's properties object. At the depth limit an instance printed as `[Object]` where node prints `[Foo]`. And the three questions in `OrdinaryHasInstance` were being asked in the order that reads best rather than the order the standard gives, which makes `0 instanceof F` throw when `F.prototype` is a number and calls a getter the language says is not called.

The first version of that branch cost 5 percent on `call/call_return` and 8 percent on `call/closure_call`, neither of which constructs anything at all, because the dispatch loop is one function and every arm competes for the same registers. Moving the body behind an `inline(never)` helper, which is the shape every other bulky piece of the loop already has, took it back to no signal in either direction.

### The seven error constructors

In #79. `Error`, `TypeError`, `RangeError`, `ReferenceError`, `SyntaxError`, `EvalError` and `URIError` exist, and the errors the engine throws are real instances of them rather than a shape that prints like one. A program can write `throw new TypeError(m)`, ask `e instanceof RangeError`, read `name` and `message` off the chain, and pass `{ cause }` through.

The seven share one body and differ only by name, by the prototype they hang off and by the slot on the isolate the engine reads when it throws that kind. The prototypes live in an array on the isolate rather than being read back out of the global object, so a throw does not depend on the program leaving `TypeError` alone and a realm with no builtins still throws something rather than failing to build the error it was going to throw with.

`String(err)` answered `[object Object]`, because the conversion to text never called an inherited `toString`. It is two halves now. The rules only half is what printing uses, so inspecting a value can never run the program's code, which is the property node has and one we should not give up by accident. The calling half sits behind `String(x)` and `+` and calls a `toString` written in Rust. A `toString` written in JavaScript is still not called, because that needs the dispatch loop to run a frame to completion from inside an opcode.

That calling half opens a recursion nothing else could reach: `e.name = e` and then `String(e)` recurses in Rust with nothing growing on the interpreter's stack, so the frame limit never fires and the process dies on the real one. A counter raises the same `RangeError` node raises for the same program, at 64 conversions deep, and it sits for free in padding that a new `constructing` flag had already created. That flag took the interpreter from 72 bytes to 80, which is the whole cost of this release in the hot structure, and the next byte added there costs another eight.

Printing an error matches node exactly, including the two rules that are only visible against a stackless error: node hides an own `name` or `message` that the first line already says, and prints a non-enumerable `cause` as `[cause]`. Both were measured with `node -e` rather than reasoned about.

An error still has no `stack`, so `console.log(err)` prints node's stackless bracketed form rather than a trace, and the milestone checklist line stays unticked because of it. Alongside that, `Object.getPrototypeOf(TypeError) === Error` is false, `Object.prototype.toString.call(err)` does not say `[object Error]`, `Error.name` and `Error.length` are missing because a function has no own `name` yet, and `captureStackTrace` and `AggregateError` are absent. Each of those refuses by name rather than answering wrongly.

### The crates go to crates.io

In #80. This is the first release that publishes them. `spec/16-package-layout.md` has listed the crates as a release artifact since the specification was written and nothing was uploading them, so a tag built five binaries, attached them to a GitHub release and stopped.

It is one `cargo publish --workspace`, which takes the order from the dependency graph and verifies the whole set builds as if it were already published before it uploads any of it. Everything about a publish that can be checked without a tag is checked on every commit instead, because a version on crates.io can be yanked but never replaced: `cargo xtask release` fails on a version that is not in lockstep, on an internal dependency asking for a version other than the one being released, and on a missing description, license or readme, and CI packages every crate the way crates.io would. The tag goes through the same check on the way in.

That check found something on its first run. Every entry in `[workspace.dependencies]` asked for 0.1.5 while the workspace was at 0.1.6, which is invisible in a workspace build because cargo resolves a path dependency by path, and the first person to run `cargo add katsu-runtime` would have been the one to find it.

The token that authorizes the upload is a secret on a GitHub environment that takes deployments only from a tag matching `v*`, so nothing running on a branch or on a pull request can reach it. It is a long lived token only because crates.io cannot attach a trusted publisher to a crate that has never been published, and #78 is what replaces it now that these crates exist.

### Where the numbers stand

Nothing in this release could be measured on either reference machine, and that is worth saying plainly rather than quoting a median that reads well. gamingpc is swinging by more than a factor of two on identical binaries at the moment: three alternating runs put main's own `call/fib` at 1,261, 1,485 and 1,475 microseconds and its own `native/native_call` at 16.0, 30.2 and 30.7. The m4 is quieter and still not quiet enough, with main's own spread at 10.52 to 17.45 microseconds on `call/call_return` and 209 to 321 on `exceptions/engine_error_caught` across four runs with nothing else on the machine.

The tell is that last benchmark. An engine error now allocates a real object with a properties object and a prototype where it used to build a bare value, so it cannot have got faster, and the branch measured ten percent quicker than main on it. That is a measurement of the laptop and not of the change. The cost of allocating an error per throw is still owed a number on a machine that can produce one, and issue #1 in the bench repository is where that stops being an excuse.

The workload baseline for this release is measured against a released tarball rather than a local build, and the tarball it is taken from is 0.1.8, because the 0.1.7 tag published one crate out of fifteen and the entry above it says why.

### Where the conformance number stands

12.31 percent, 9,995 cases of the 81,225 attempted, up from 6.66 percent and 5,410. It moved twice. `new` took it to 8,924, and the error constructors took it to 9,995, mostly through the harness rather than through the error tests: `assert.throws` reads `name` off the constructor it was handed and `propertyHelper.js` asks `e instanceof TypeError`, and 20,360 of the 53,874 JavaScript files under test262's `test/` mention an error constructor or `assert.throws`.

The differential harness against node v26.7.0 is at 2,017 programs, 2,016 agreed, 0 differed and 1 untested, and the one gap is still `set_index`, which is element writes.

## 0.1.6

The sixth patch release of M1, three pull requests on from 0.1.5, and two of them are one piece of work: a function is an object now. It can carry properties, so `Foo.prototype` and the statics on a constructor are ordinary properties in an ordinary object, and `Object` is a function rather than a namespace object wearing the wrong type tag. This is the wall that `new` has been standing behind, and it is the reason 75,807 test262 cases stop where they stop.

### A function carries its properties beside it

In #73. A property written on a function is kept, read back and printed, where before the write was dropped and the read answered `undefined`.

The properties go in an ordinary object that the function points at, rather than in the function itself. The first word of everything in this cage is either a shape or a kind tag, and that one test is how the heap tells a closure from an object without a second dereference. Giving a closure a shape there, which is what a mature engine does, would turn every kind question into a read through the shape to re-derive what a tag already says, on the hottest path in the runtime. A side object costs one pointer chase on the first property access instead, and after that the prototype chain and the inline caches work on it exactly as they work on anything else, so a read of `Foo.prototype` caches the way a read of `o.x` does.

A closure went from sixteen bytes to twenty and a native from eight to twelve, and the field is zero until something asks for it. A function that is only ever called never allocates the object, which is most functions in most programs. The one that is read from builds it and builds `prototype` with it, because a function somebody reads a property off is usually a constructor, and puts `constructor` on that prototype pointing back at the function.

The read path does not move: `property/prop_load_hot` measured 8.19 us on both sides of the change on gamingpc, which is what inlining the probe and making the function case a cold call out of it was for. The store path costs 7 percent and it is worth being exact about what that 7 percent is. It was 12 percent when the object test sat behind a helper answering for both cases, and rewriting the arm so the ordinary object is tested first got a third of it back. The rest is not work a store does: a third build, identical except that the store path never asks about functions, measures main's number, so a call the store never makes is costing it something through the register allocator. Taking that back means giving the whole cold tail its own function, and it is worth doing when there is a setter on `Function.prototype` to test it against.

### `Object` is a function, and so is `Function`

In #74. `typeof Object` answers `function` where it answered `object` for the last four releases, `Object` prints as `[Function: Object]`, and `Object.getPrototypeOf(Object)` finds the same `Function.prototype` that sits above any function a program writes. It carries its six statics as ordinary properties, which is only possible because of the change above. `Object(x)` works as a call, and `Object.prototype.constructor` is there, so `({}).constructor === Object` is true.

That last line is what pulled `Function` into the same release. `constructor` on `Object.prototype` is inherited by everything in the realm, functions included, so the moment it existed a plain `function f() {}` answered `f.constructor === Object` where node answers `Function`. A wrong answer is worse than a missing one, so `Function`, `Function.prototype` and its own `constructor` came with it, found first because that prototype sits below `Object.prototype` on a function's chain. Calling `Function` refuses by name, because its last argument is source and that is a compiler entry point carrying every question `eval` carries about which scope the result closes over.

A function answers the same questions about its own properties that an object does. `Foo.hasOwnProperty('bar')`, `Foo.propertyIsEnumerable('prototype')`, `Object.defineProperty(Foo, 'hidden', {value: 2})` and `Object.getOwnPropertyDescriptor(Foo, 'prototype')` all work, and every descriptor involved was read off node rather than remembered. Inside the engine the embedder API learned to see through a function to its properties: the three questions that only look find them without building anything, and the one that defines builds them.

`Object.create(f)` refuses by name rather than answering wrongly, and the reason is the shape again. A prototype link points at an ordinary object and a function keeps its properties beside it, so the link would point at the side object and `Object.getPrototypeOf` would hand back something that is not the function. That is the kind of nearly right answer that costs a day to find later.

Nothing in the dispatch loop changed and the numbers say so: `property/prop_load_hot` 8.31 us to 8.16 us and `property/prop_store` 9.10 us to 8.67 us on gamingpc, three runs a side alternating, with startup unchanged at about 2.1 ms. The first attempt at those numbers showed a 23 percent regression that did not reproduce on a second round, which is worth knowing about that machine: a single pair on it can be wrong by 40 percent, and turbo needs pinning before any number from it can carry the 10x claim.

### Where the numbers stand

`fib.js` from tamnd/katsu-bench, 25 runs of each runtime after 3 discarded, interleaved in one session on the same m4, timed by the workload's own `performance.now()` rather than by wall clock around the process. These are the figures from the published 0.1.6 baseline in that repository, measured against the released tarball rather than a local build, so they are the ones a reader can rerun.

| Runtime | fib(35) compute | Peak memory | Against katsu |
|---|---:|---:|---|
| katsu 0.1.6 | 875.16 ms | 2.70 MiB | |
| node 26.7.0 | 53.38 ms | 46.34 MiB | 16.4x faster, 17.2x heavier |
| bun 1.4.0 | 34.28 ms | 18.03 MiB | 25.5x faster, 6.7x heavier |
| deno 2.9.6 | 56.54 ms | 36.06 MiB | 15.5x faster, 13.4x heavier |

The ratio against bun went from 23.6x to 25.5x and this release is the one where that gets a straight answer rather than the usual sentence about the laptop. Two things are true at once. The session was slower for everybody, node by eight percent and deno by thirteen and bun by two, and katsu moved eleven, which is inside that band rather than outside it. And there is a mechanism this time, which is that a closure grew from sixteen bytes to twenty, so `fib` pays four bytes on each of the two functions it defines even though it hangs nothing off either of them. The gamingpc microbenchmarks put the call group inside noise across that change with one run of six showing three percent. A couple of points of the eleven are plausibly real and the rest is the machine.

A head to head between the 0.1.5 and 0.1.6 release binaries was run to settle it and could not, because the laptop's load average went past 26 while it ran and `fib` under one binary varied between 2.4 and 5.9 seconds inside a single alternating sequence. That is published in the bench repository rather than left out, and it is the clearest argument yet for moving these numbers onto dedicated hardware.

Memory is unchanged at 2.70 MiB against bun's 18.03, which answers the obvious worry about a release that made two heap objects bigger. `fib` allocates almost nothing either way, and the row that will actually test a bigger closure is the one that allocates in a loop, which is `alloc.js` and still does not run. Still one of six compute workloads running, and what stops the other five is unchanged: `alloc.js` and `sort.js` want `new`, `json.js` and `nbody.js` want array literals, and `strings.js` wants a collector.

### Where the conformance number stands

6.66%, 5,410 cases of the 81,225 attempted, unchanged for the fifth release running. 75,807 cases still stop on the `new` in `throw new Test262Error(message)`. This is the last release where that sentence is an explanation rather than an excuse, because everything `new` was waiting for is now here.

### Also

#72 replaced the fib absolutes in the 0.1.5 entry with the published baseline for the same release, because the changelog had quoted a busier session than the one katsu-bench published and one release should not carry two sets of absolutes.

The differential harness is unchanged at 1,015 programs with 1,014 agreements, 0 differences and 1 gap, which is `set_index` and therefore element writes.

## 0.1.5

The fifth patch release of M1, three pull requests on from 0.1.4, and all three are one arc. A property slot can now hold a pair of functions, an object literal can write that pair, and a property read remembers where it found what it found. The first two are language features that each cost about a fifth of a property read, and the third is the thing that takes both of them back and more, because it is what the shape was being built for all along.

### A property can be a pair of functions

In #68. A read or a write on a property can be a call now, so a getter runs when its name is read and a setter runs when its name is written.

The pair is one boxed heap object and not two slots. A property is one slot everywhere in the heap and making it sometimes two would put a width question on the plain property path, which is the path that has to be fast, and buy nothing on the path that is already a call. The fourth attribute bit saying which kind a property is went into the same shape padding the first three did, so the flags still cost nothing.

The receiver is where the access started and not where the property was found. That is the whole reason to put an accessor on a prototype: one getter above answers for every object below it and each of them sees itself as `this`.

A setter is the odd case in the interpreter, because its answer is thrown away and the operands of the store that called it are still live. It returns into a register that is nowhere, and `Register::NOWHERE` is a sentinel rather than an `Option` for the same reason the receiver is one.

The rules were run under node case by case rather than remembered. Printing shows `[Getter]`, `[Setter]`, `[Getter/Setter]` or `undefined` when a property has neither half. A second `defineProperty` keeps the half it does not mention. An accessor turned into a data property comes out not writable with its other two flags intact. Writing to something that has only a getter is swallowed in sloppy mode and refused in strict with node's exact wording.

### A literal defines, it does not assign

In #69. `get x() {}` and `set x(v) {}` work in an object literal, and two halves written under the same name join into one property rather than replacing each other, in either order and with other properties in between.

Writing the syntax turned up something else that was wrong and had been wrong since literals existed. Every property in a literal defines rather than assigns, which is what the language has always said and what katsu was not doing. It was measured rather than reasoned about: with a setter installed on `Object.prototype`, node does not call it for `{p: 1}` and does call it for `q.p = 2`, and a non writable property up the chain stops the assignment and not the literal.

So a literal no longer asks the prototype chain for permission, and it can put a value over an accessor the way `{get x() {}, x: 5}` has to. It made literals faster rather than slower, which is the direction that follows from skipping a prototype walk: a two property literal and a four property one are both about six percent quicker over twenty alternating pairs, and building the same four property object out of four assignments is unchanged because those are still assignments.

### Property reads have inline caches

In #70, and this is what the last four pieces of object model work were aimed at. Each access site remembers one shape and the position the name was found at, and a site whose shape matches skips all three of the costs that had been accumulating. No walk up the shape's parent chain comparing interned names, because the shape says where the property is. No read of the attributes to find out whether the slot holds a value or an accessor pair, because the flags are part of what the shape says. No walk up the prototype chain when the object does not have the name itself, because the prototype is in the shape too. One comparison settles all three.

An entry holds a property position rather than a byte offset, and that is the one part of it that was measured into shape rather than designed. An offset is a load cheaper on a hit because it says outright where the value is, and it does not work on its own: `{a: 1}` is built with room for one property and `x = {}; x.a = 1` is built with room for none and keeps its property in the overflow array, and both reach the same shape. An offset therefore has to be guarded by the inline capacity as well, which makes the key eight bytes instead of four, an entry sixteen instead of eight, and every object built the second way a permanent miss. That version was slower on every benchmark it was not faster on, including the store benchmark, which reads no cache at all and was paying only for a table twice the size it needed to be.

A cached property read costs 9.5 ns against 12.1 ns before, a little under a quarter off, subtracting the global lookup and the loop out of `property/prop_load_hot` over ten alternating pairs. The benchmark line itself moves 14 percent, because half of what it does is look up `host` rather than read a property off it.

What a cache costs when it cannot help is the other half of the answer and it belongs here rather than only in a benchmark file. `property/prop_load` is a thousand sites each run exactly once, so every read fills an entry nothing ever reads back, and it is 22 percent slower. `property/prop_store` is 5 percent slower for a different reason, which is that a site is given its eight bytes when the program loads whether or not anything ever fills them, and stores do not fill them yet.

The cache is monomorphic, which is one shape and not the four the design calls for, so a site that alternates between two kinds of object pays the full search every time and pays to fill an entry as well. Own properties only, because caching an inherited hit needs a validity cell for the holder's own shape. Reads only, because a store that grows an object changes its shape and so wants a transition rather than a position.

### Where the numbers stand

`fib.js` from tamnd/katsu-bench, 25 runs of each runtime after 3 discarded, interleaved in one session on the same m4, timed by the workload's own `performance.now()` rather than by wall clock around the process. These are the figures from the published 0.1.5 baseline in that repository, measured against the released tarball rather than a local build, so they are the ones a reader can rerun.

| Runtime | fib(35) compute | Peak memory | Against katsu |
|---|---:|---:|---|
| katsu 0.1.5 | 789.33 ms | 2.72 MiB | |
| node 26.7.0 | 49.27 ms | 46.36 MiB | 16.0x faster, 17.0x heavier |
| bun 1.4.0 | 33.47 ms | 18.05 MiB | 23.6x faster, 6.6x heavier |
| deno 2.9.6 | 49.99 ms | 35.62 MiB | 15.8x faster, 13.1x heavier |

The ratio against node went from 16.6x to 16.0x and against bun from 24.0x to 23.6x, and neither is a result. For once the two sessions are directly comparable, because the rivals barely moved between them: node measured 49.5 ms in the 0.1.4 session and 49.27 ms here, bun 34.2 and 33.47. Against that the katsu absolute went from 821 ms to 789 ms, four percent, which is smaller than the spread between sessions and has no mechanism behind it.

The reason it has no mechanism behind it is the same reason nothing moved last time. `fib` is a call benchmark wearing an arithmetic benchmark's clothes, and it does not read properties, so an inline cache for property reads has nothing to work on there. The microbenchmarks that do cover this release moved and are quoted above.

The memory column is new here and it is the first row of the second half of the goal. katsu runs `fib` in 2.72 MiB of peak resident memory against bun's 18.05, which is 6.6 times less, and it does that with no collector at all, so it is a fact about how little the interpreter allocates rather than about how well it cleans up. It will get worse before it gets better, because `strings.js` still fills the 4 GiB cage and dies for exactly that reason.

Still one of six compute workloads running, and what stops the other five is unchanged. `alloc.js` and `sort.js` want `new`, `json.js` and `nbody.js` want array literals, and `strings.js` wants a collector.

### Where the conformance number stands

6.66%, 5,410 cases of the 81,225 attempted, unchanged for the fourth release running. 75,807 cases still stop on the `new` in `throw new Test262Error(message)`, one fewer than last time, and the accessor work in this release is part of what it takes to get past that line rather than something that could have moved it on its own.

### Also

The differential corpus gained `literals.js`, and a full run is 1,015 programs with 1,014 agreements, 0 differences and 1 gap, which is `set_index` and therefore element writes.

## 0.1.4

The fourth patch release of M1, three pull requests on from 0.1.3, and all three are the object model. An object now has a prototype chain that is really walked, properties that know what they are allowed to do, and a `this` that is bound where the call happens. The theme underneath all three is that the shape carries the answer, so that an inline cache which has compared one shape has in that one comparison also checked everything else it needed to know.

### The prototype lives in the shape

In #64. A property that is not on an object is now looked for above it, all the way up rather than one step, and `Object.create` and `Object.getPrototypeOf` are there to build a chain and to read one.

Keeping the prototype in the shape rather than in the object is the decision the rest of the object model is aimed at. An inline cache that has compared one shape has in that single comparison also checked every prototype between the object and wherever the property was found, so an inherited property is guarded exactly as cheaply as an own one. It is what V8 does with the map and what JavaScriptCore does with the structure, for the same reason in all three places.

A write always makes an own property and leaves the prototype alone, because there are no setters yet for it to reach.

### Properties know what they are allowed to do

In #65. A property carries three flags, and `Object.defineProperty`, `Object.defineProperties` and `Object.getOwnPropertyDescriptor` set them and read them back.

The flags live in the shape next to the prototype, on the node that added the property, and they became part of a transition's identity. Adding `x` as a plain property and adding `x` as a hidden one are two different edges out of the same shape, because two objects that differ in what a `for in` sees are not two objects with one layout. It cost nothing: a shape asked for 28 bytes and the heap reserved 32 for alignment, so the flags went into padding that was already being paid for.

Defining a property is not assigning to one, and almost nothing about the two is the same. Assignment asks the prototype chain for permission and a definition does not, a definition can change what a property is allowed to do and an assignment cannot, and a definition leaves out any flag it was not given rather than defaulting it. So `o.x = 1` makes a property that can do all three things and `Object.defineProperty(o, 'x', {value: 1})` makes one that can do none of them.

A read only property on a prototype stops a write to every object below it, even though none of those objects has the property, because the chain is searched before the write and what is found there decides the answer. That search made stores faster rather than slower, by about a tenth, because the write now goes straight to the slot the search returned instead of looking the name up a second time.

The rules for redefining were run under node case by case rather than remembered. A non configurable property can only ever become less permissive, a non writable one can be redefined to the same value and not to a different one, and the comparison is `SameValue`, which is why a non writable `NaN` can be redefined to `NaN` and a positive zero cannot be redefined to a negative one.

### this is bound at call sites

In #66. A method call knows the object the method was read from and carries it onto the frame the call pushes. That is what `CallMethod` was always for, and it is why the property read and the call are one opcode rather than two: keeping them together is the only way the receiver survives between them.

Nothing supplied is not the same as `undefined` supplied, and that third state is the whole design. What `this` means in a plain call depends on where the code is and whether it is strict, both of which are properties of the callee rather than of the caller, so the call site cannot answer it and `Op::LoadThis` can. A strict function called on nothing gets `undefined`. A sloppy one gets `globalThis` and there is no global object yet. The outermost frame in a file gets `module.exports` and there is no module system yet. The last two refuse by name rather than answering `undefined`, because `undefined` is a legal answer that a program will act on and a refusal is not.

That third state ended up costing nothing, after first costing something, and the detour is worth recording because the same trap waits in every frame field added from here. An `Option<Value>` is what the type wants and it took the frame header from 24 bytes to 40, because a NaN boxed value has no spare bit pattern for a discriminant to hide in, and it measured 14 percent on `call/call_return` and on `call/fib`. `Value::EMPTY` is what the encoding already uses for "there is no value here", no call site can produce it, and it says the same thing in the eight bytes the value was going to take anyway.

`Object.prototype` has methods on it now, which is what receivers were blocking: `hasOwnProperty`, `isPrototypeOf`, `propertyIsEnumerable`, `toString` and `valueOf`. All five are non enumerable, writable and configurable, which is what node reports for each of them, and that is not a detail to wave through, because they sit at the top of nearly every prototype chain in the realm and an enumerable one would appear in every `for in` over every object in the program. Every one of them begins with `ToObject(this)`, which throws in node's exact words for `undefined` and `null`, returns an ordinary object unchanged, and refuses by name for anything that would need a wrapper prototype.

Accessors are still not here, and the reason changed. They were blocked on receivers and they are now blocked on storage: a property slot holds a value rather than a pair of functions, so a getter needs the slot to hold a pair and a shape node needs a flag saying that it does. The refusal message says so.

### Where the numbers stand

`fib.js` from tamnd/katsu-bench, medians of five runs of each runtime interleaved in one session on the same m4, timed by the workload's own `performance.now()` rather than by wall clock around the process.

| Runtime | fib(35) compute | Against katsu |
|---|---|---|
| katsu | 821 ms | |
| node v26.8.1 | 49.5 ms | 16.6x faster |
| bun 1.4.0 | 34.2 ms | 24.0x faster |
| deno 2.9.6 | 51.3 ms | 16.0x faster |

The absolute improved from 1,055 ms and the ratio did not, and the second half of that sentence is the real one. Node measured 63 ms in the 0.1.3 session and 49.5 ms here, which is the same 21 percent the katsu number moved by, so what changed between the two sessions was how busy the laptop was rather than how fast the interpreter is. The ratio against node went from 16.7x to 16.6x, which is nothing, and it is nothing for a good reason: `fib` is a call benchmark wearing an arithmetic benchmark's clothes, and none of the three pull requests in this release touched the call path or the arithmetic. They touched properties, and `fib` does not have any. This is what a paired ratio is for and it is why the table is read down the last column.

The two changes that were expected to move a number did, on the microbenchmarks that actually cover them. `property/prop_store` got about a tenth faster from the chain search in #65. `stack/push_pop` got 15 percent slower from the frame header growing 24 bytes to 32 in #66, measured over forty ABBA pairs, and it is the only benchmark that moved: it pushes and pops a frame and does nothing else, so it is the one place where eight more bytes of header is the entire workload. `call/call_return` at 0.999 and `stack/call_and_return` at 0.983 say that anything which then runs a call amortises it away.

Still one of six compute workloads running, and what stops the other five is unchanged: `alloc.js` and `sort.js` want `new`, `json.js` and `nbody.js` want array literals, and `strings.js` fills the 4 GiB cage and dies because there is no collector.

### Where the conformance number stands

6.66%, 5,410 cases, unchanged again and expected to be. 75,808 cases still stop on the `new` in `throw new Test262Error(message)` before reaching any line the object model could answer. Three releases have now described the same wall, and the object model work in this one is part of what it takes to get past it, since a constructor needs a prototype to hang off and `new` needs somewhere to put it.

### Also

The differential corpus gained `receivers.js`, and a full run is 1,013 programs with 1,012 agreements, 0 differences and 1 gap. One divergence in the receiver area was deliberately kept out of the corpus because it is pre-existing rather than new: node says `bare.toString is not a function` where katsu says `toString is not a function`, since katsu does not keep call site source spans yet.

## 0.1.3

The third patch release of M1, three pull requests on from 0.1.2, and all three exist so that a program can measure itself and say what it found. `performance.now()` and `performance.timeOrigin` are there, `String()` and `JSON.stringify()` are there, and with those four names the `fib` workload in tamnd/katsu-bench runs end to end. This release is the first one that publishes a compute number about katsu, and the number is bad.

### A program can time itself

`performance.now()` and `performance.timeOrigin`, in #59. Not ECMAScript, it is the W3C High Resolution Time specification, and it is here for the same reason `console` is: from a program's point of view it is simply there, and every benchmark harness worth reading uses it rather than `Date.now()`, which is whole milliseconds off a wall clock that can move backwards under an NTP correction while the work being timed is still running.

It takes two clocks and the pair is not redundant. Elapsed time comes from the monotonic clock, because that is the only one that cannot go backwards. The origin comes from the wall clock, because `timeOrigin` is defined as milliseconds since the Unix epoch and a monotonic clock has no epoch, its zero being the boot time on Linux and something undocumented on macOS. The two are read within nanoseconds of each other at startup so that `timeOrigin + now()` lands on `Date.now()`, and there is a test that asserts exactly that, because it is what catches an origin taken from the wrong clock.

The origin is stamped from the first statement of `main`, before argument parsing and before the logger. That is a deliberate choice about what a program is allowed to see: it puts katsu's own startup inside katsu's own numbers rather than excluding it. An embedder that calls the library instead of the binary gets a lazy stamp on the first read, which is the right answer there for a different reason, since a host process that has been up for six hours has a process start that is not the runtime's beginning.

Nothing is coarsened. Browsers round `performance.now()` to five microseconds because a high resolution timer in a page shared with an attacker is half a Spectre gadget, and that reasoning does not apply to a program you chose to run. Node does not coarsen either, measuring about 400 ns between consecutive calls, and katsu matches node.

### A program can print what it computed

`String()` and `JSON.stringify()`, in #62.

`String(x)` does not carry its own copy of the conversion rules. `coerce_to_string` was split into the half that allocates and `text_of`, which is the rules, and `String(x)` and `'' + x` now go through the same `text_of`. These are one conversion in the language, the number to text half of it is the shortest round tripping decimal with ties broken towards even, and the differential harness has already caught us getting that wrong once. Two copies would eventually be two answers. There is a test that walks every value this build can produce and asserts the two agree on all of them, with `-0` in the list on purpose because it is the value most likely to make them diverge.

The two cases where `String(x)` and `console.log(x)` are specified to differ are tested from both sides. `String(-0)` is `0` and the console prints `-0`, because a console that cannot tell you which zero you have is hiding the thing you turned it on to see. `String({a: 1})` is `[object Object]` and the console prints `{ a: 1 }`.

Every rule in `JSON.stringify` was run under node v26.8.1 and copied down rather than remembered, which caught four things that are easy to get wrong from memory. The indent clamps at ten rather than being unbounded, in both its number and its string form. An empty object stays `{}` on one line even when an indent was asked for, and so does an object whose every property turned out to have no JSON spelling. A property whose value is `undefined` or a function takes its name with it, so a reader cannot tell it apart from a property that was never there. And a cycle is a `TypeError` reading "Converting circular structure to JSON", node's exact sentence, so a program matching on the message cannot tell which engine it is under. The cycle check is a stack of the objects currently being written rather than a set of the objects seen, which is the difference between a real cycle and the same object appearing twice side by side, and that distinction has its own test.

### Refusing by name

There is a new error variant, `Unsupported`, and it changes how gaps in the standard library get reported from here on. `NotImplemented` names an opcode and a function written in Rust has no opcode to point at, so until now a half built builtin had no way to say whose gap it was.

`JSON.parse` is the first user. It is present and refuses instead of being absent, because a missing method arrives as `JSON.parse is not a function`, which is an ordinary JavaScript error that a program will feature detect around and a reader will take for a bug in their own code. It is deliberately not catchable, and that is the sharper half of the argument: wrapping `JSON.parse` in a `try` is how everybody writes it, because malformed input is the expected case, so a catchable gap of ours would be read as bad input and the program would go down its error path with an answer that looks reasonable and is wrong.

### Where the numbers stand

The first compute number this project has published about itself. `fib.js` from tamnd/katsu-bench, medians of five consecutive runs of each runtime on the same m4 in the same session, reported by the workload's own `performance.now()` timing rather than by wall clock around the process.

| Runtime | fib(35) compute | Against katsu |
|---|---|---|
| katsu 0.1.3 | 1,055 ms | |
| node v26.8.1 | 63 ms | 16.7x faster |
| bun | 45 ms | 23.4x faster |
| deno | 68 ms | 15.5x faster |

That is the whole of it and it is not close. `fib` is a call benchmark wearing an arithmetic benchmark's clothes, one comparison and one addition per call and nothing else, so it punishes exactly the three things this build does not have, which are inline caches, shape guards and a JIT. Two of those are on the M1 list and the third is M3. The number is published rather than held back until it improves, because the 10x goal has to be measured from a real starting point and a baseline nobody publishes is a baseline nobody is held to.

One of the six compute workloads runs, up from none. What the other five stop on is three known pieces of work rather than five: `alloc.js` and `sort.js` want `new`, `json.js` and `nbody.js` want array literals, and `strings.js` fills the 4 GiB cage and dies because there is no collector.

Startup is measured differently now and by the runtime itself, since `performance.now()` at the first line of a program is exactly the question "how long did this runtime take to get here". katsu reaches it in 0.52 ms against node's 36.2 ms and bun's 9.9 ms, medians of five with the cold first run dropped. The m4 was loaded during this session and every absolute in that sentence is higher than the same machine gives when it is quiet, which is why the ratio is the part to read.

An attempt at microbenchmarking the two new builtins from JavaScript was made and thrown away rather than published. Twenty identical two hundred thousand iteration runs of the same loop, back to back in one process, ranged from 66 ns to 888 ns per iteration and came back down again, which is a busy laptop and not a finding. Those numbers belong on the pinned hardware the benchmark repository is being pointed at, and they will be taken there rather than guessed at here.

### Where the conformance number stands

6.66%, unchanged, and it was expected to be unchanged. 75,807 cases still stop on the `new` in `throw new Test262Error(message)` before they reach any line a builtin could answer, so a standard library addition cannot move this number until constructors land. That is the same wall the last two releases described and it has not moved because nothing in this cut was aimed at it.

### Also

The differential harness got a tenth corpus file, `serialization.js`, sixty assertions on `String` and `JSON.stringify` run against node directly. The generator does not emit either call, so the property based half of the harness would not have covered any of this on its own. A full run is 2,009 programs and 2,009 agreements.

The node oracle in that harness now gets thirty seconds rather than five, in #61, and the reason is worth writing down because it is not the obvious one. Five seconds was never a budget for how long a program takes to run, it was a budget for how long a process takes to exist, and spawning node means the operating system opening a hundred and thirty nine megabytes of executable on a shared runner with an antivirus that reads the whole file first. It produced a false failure on Windows where katsu printed the right answer for `console.log(0.1 + 0.2)` and node never printed anything, and the harness reported that as node breaking. Catching a real hang thirty seconds late costs one run. Failing a green build because a shared runner stalled costs the harness its credibility.

## 0.1.2

The second patch release of M1, five pull requests on from 0.1.1, and all five are control flow. Exceptions run, `finally` runs, strict mode takes names away before a program starts, every counting loop runs, and a `break` or a `continue` can name the loop it means. Between them these are the last pieces of statement level JavaScript that need nothing from the object model, so what stands in front of the rest of the language now is prototypes rather than more grammar.

### Exceptions

`throw` is one opcode, a `try` is no opcodes at all, and where a throw goes is decided by walking a handler table the frontend wrote, in #52. That is the trade the JVM takes and for the same reason: a `try` inside a hot loop is common and a throw inside one is not.

Lowering pushes a table entry after it has finished the block that entry protects, so a nested `try` lands earlier in the table than the one around it, and "the first entry containing the throwing instruction" and "the innermost handler" become the same sentence rather than two rules that have to be kept in step. The range is half open and stops before the handler starts, which is what makes a `throw` written inside a `catch` not caught by the `catch` it is written in. The search crosses frames: a frame with no handler for the instruction it stopped at is popped, and the frame underneath resumes at the call that got it there.

Three errors are not catchable, and it is a list one function checks rather than a flag on each variant: out of memory, an interrupt, and an opcode this build has not written yet. The first two are not catchable under node either, and the third is a gap in katsu rather than an event in the program, so letting a `catch` swallow one would turn a missing feature into a wrong answer. A stack overflow is catchable, because node reports it as a `RangeError` a `try` can take.

An engine error stays a name and a message until something catches it and becomes an object with those two as own properties at the moment a handler takes it, so the allocation is paid at the `catch` rather than at the throw. It is not an `Error` yet in the sense that there is nothing for it to be an instance of, so `e.name` and `e.message` read correctly while `e instanceof Error` and `e.stack` wait on prototypes and source spans.

### `finally`

A `catch` runs on one way out of a block and a `finally` runs on all five, so it could not be another entry in the handler table, which only knows about throwing. What it is instead, in #53, is a body with one entry point reached from the normal path and from every abrupt one, and a dispatch after it that sends the completion on to wherever it was going. Two registers carry that: a token saying which of the five ways out this was, and a payload holding what the completion carries. A normal completion sets the token to zero and zero is falsy, so the dispatch is one `jump_if_false` on the path nearly every `finally` takes, and a throw needs no jump into the body at all because the handler entry names the body's prologue directly.

The dispatch only asks about the completions that actually routed through, so a `try` and `finally` with nothing abrupt inside it has no comparison anywhere, and the last kind never needs a test because a token that is not zero and is not any of the kinds already asked about can only be the one remaining.

The override rule fell out of the frame ordering rather than being written. The frame is pushed while the protected block is lowered and popped before the body is lowered, so a `return` written inside a `finally` is lowered against whatever is outside the construct and leaves rather than routing back into the body it is written in, which is what makes `try { return 1; } finally { return 2; }` answer two. Nesting needs nothing that knows about nesting either, since each dispatch arm re issues its completion through the same three functions an ordinary statement goes through. No opcode was added and the interpreter did not change by one line, which is what the instruction set spec predicted when it dropped `ReThrow`.

### Strict mode takes names away

`"use strict"; var public = 1;` and `"use strict"; eval = 42;` are both `SyntaxError`s before a line of either program runs, and until #54 we ran them happily. There are two rules here and they are not the same rule. Nine words stop being identifiers entirely, `implements`, `interface`, `let`, `package`, `private`, `protected`, `public`, `static` and `yield`, and reading one is enough, so `public;` on its own is an error even though there is nothing to bind. `eval` and `arguments` stay perfectly good names to read and are refused only where a program moves one, which is a `var`, a `let`, a `const`, a function name, a parameter, a catch parameter or an assignment target.

Two words are missing from the nine on purpose. `enum` is reserved in both modes rather than only in strict code, so the parser rejects it before the adapter sees it, and `await` is not a strict mode reserved word at all since what makes it special is a module or an async function. A property name is never checked by either rule, so `o.public` stays legal. The function name is checked against the function's own strictness rather than the enclosing one, so `function eval() { "use strict"; }` is refused from a sloppy file, which is why that check sits after the body's directives have been read.

Five test262 cases went straight from failures to passes, all five directive prologue tests that write a strict mode violation after a `throw` that must never be reached, so they only pass if the violation is found before anything runs.

### Every counting loop

`while` was the only loop this engine ran until #55, so `for (let i = 0; i < n; i++)` was refused by name and so was `do { } while (c)`. They are three layouts and not one layout with a flag, because the whole difference between them is where the test sits relative to the body. A `do while` puts the body at the top and its `continue` goes to the start of the test rather than to the back edge, since landing on the back edge would skip the condition and turn `do { continue; } while (false)` into an endless loop. A `for` is head, test, body, update, back edge, so its `continue` lands on the update, which is what makes a `continue` in a counting loop terminate when the same `continue` in the `while` it desugars to would not.

Every part of a `for` head is optional and every part that is absent costs nothing. An absent test emits no comparison at all rather than a comparison against a constant, so `for (;;)` is a tighter loop than `while (true)` and needs no implicit return after it. The head opens one block scope covering the init, the test, the update and the body, and it is declared before its own initialiser is walked, which is the only reason `for (let i = i; ;)` is a dead zone error rather than a read of an outer `i`.

One thing about loops is knowingly wrong and it is written down rather than left to be found. A `let` head gets one binding per call and not one per iteration, because environments here are per function, so `for (let i = 0; i < 2; i++)` with a closure made in each iteration gives `0 1` in node and `2 2` here. It reproduces on a `while` around a block too, so it is not something the loop work introduced, and fixing it needs block level environments.

### Labels

A label was refused by name until #56, so the only way out of a nested loop was a flag and a second test, and there was no way at all to leave a plain block early. This is a change to the frame stack rather than a new statement shape: a loop and a switch already push a frame that collects the jumps aimed at them, so a label written on one is handed to that frame instead of building a second one around it, a label on anything else pushes a frame that collects breaks and nothing else, and a jump searches outwards for the frame wearing the name rather than taking the nearest one.

The case that needed more than a search is a labelled jump leaving through a `finally`, because by the time the dispatch after the body runs, the frame the jump was aimed at is gone. Each distinct labelled target routing through one `finally` gets a token value of its own, allocated per function and deduplicated, so two different labelled breaks through one `finally` end up in two different places and a `finally` with no labelled jump through it compares exactly the numbers it always did.

Labels and variables turn out to be two namespaces that never meet, so `let x = 1; x: while (0);` is legal, and a duplicate label is an early error only when one encloses the other. Every rule was measured against node rather than remembered, and the three early error messages are node's word for word.

### Where the numbers stand

Exceptions, measured per loop iteration on `gamingpc-win` pinned to one performance core. The table is on that machine rather than the m4 because the m4 was indexing photos throughout the session and moved by more between reruns than several of the differences below.

| Operation | gamingpc-win |
|---|---|
| An iteration of a plain loop, for scale | 14.29 ns |
| The same iteration wrapped in a `try` that never fires | 15.14 ns |
| The same iteration wrapped in a `finally` that always runs | 19.09 ns |
| An iteration that throws a number and catches it in the same frame | 16.36 ns |
| The same throw travelling through one `finally` on its way out | 27.76 ns |
| A call and a `return`, for scale | 23.98 ns |
| The same `return` routed through a `finally` | 41.57 ns |
| A throw caught three frames up | 51.54 ns |
| A caught `TypeError` instead of a caught number | 196.58 ns |

Entering a `try` costs 0.85 ns, which is one `jump` per exit over the handler and one register of frame width, and both go away by lowering the handler out of line. A `finally` that never fires abruptly costs 4.80 ns over a plain loop, which is three instructions on the normal path plus two registers, and against 1.55 ns per instruction that is the whole of it, so nothing is hiding. It is about five times what a `catch` costs, and the reason is not the token, it is that a `catch` gets to be zero instructions on the path that does not throw and a `finally` cannot be.

Two numbers are worth acting on. Routing a `return` through a `finally` costs 17.59 ns, and most of it is a `strict_equal` going through an inline cache and a general comparison in order to ask whether a small integer written one instruction ago equals a small integer constant. A compare against an immediate would answer that without either, and that opcode does not exist because nothing needed it before. And a caught `TypeError` is 180 ns more than a caught number, most of which is a `format!` and a `String` built at the throw site before anything knows whether a handler exists, which would move behind the same test that already defers the object.

Unwinding got sharper on a machine that can be pinned. Three frames of distance costs 35.18 ns and three calls on the same machine in the same session are 33.30 ns of it, so a frame pop and a missed table is about 0.6 ns rather than the 3.4 ns the noisy m4 numbers suggested.

### Where the conformance number stands

It moved from 6.65% to 6.66%, which is 5,410 of the 81,225 cases attempted, and the shape of that is more useful than the figure. 75,807 cases stop on the same line of the suite's own harness, `throw new Test262Error(message)`. The wall used to be the `switch` above that line, then the `try` on it, and it has now moved along the line to the `new`. Each statement implemented moves it by one construct rather than knocking it down, and the one left needs constructors and therefore prototypes, so for the first time the answer is a piece of the object model rather than more grammar.

Two of the five releases in this cut moved the number by nothing at all, and that was measured rather than assumed by building `main` into a separate target directory and running both. A filtered run of `language/statements/for` reports the same 75 passing with the loops implemented as without them, and `language/statements/labeled` reports the same 15 passing before and after labels. The suite does not get to see either piece of work until constructors land.

### What the harness found

Three of the five pull requests changed the differential generator, and the run is now seven thousand generated programs plus a nine file corpus, agreeing with node on every one across two runs at different seeds.

The `try` production has two decisions in it that are what make its programs able to fail. The throw is drawn rather than always emitted, because a `try` that always fires never runs the path almost every real `try` takes, and the handler assigns the caught value to a binding declared outside the `try`, because a generated handler mentions the caught name only by accident and without somewhere to put it both paths would print the same thing. The loop production draws between all three forms, and only the `for` keeps its counter in the head, because its update runs on the way round even after a `continue` skipped the rest of the body. The label productions track two lists rather than one, because every labelled statement is something a `break` can name and only the ones on loops are something a `continue` can name.

Labels also found a crash that predated them by two milestones. Annex B lets sloppy code write a function declaration as the single statement of an `if` or under a label, and node runs `if (c) function f(){}` while refusing it in strict mode. katsu panicked on all of them, because nothing declared the name and the scope pass then looked it up. It is refused by name now, which is an honest answer where a crash was telling the user nothing about their program.

## 0.1.1

The first patch release of M1, three pull requests on from 0.1.0, and between them they replace the two biggest holes in the middle of the language. There is a `switch` and there are objects.

### `switch`, `break` and `continue`

The statement form to implement first was chosen by measurement rather than by taste, in #48. A test262 run showed that 75,804 of the 75,813 cases the runner could not attempt stopped at the same `switch` in the suite's own `harness/assert.js`, so one missing statement was standing in front of five sixths of the suite.

Three things about a switch are the opposite of what the syntax suggests, and all three had to be built rather than assumed. Every clause shares one block scope rather than getting one each, so a `let` written in the last clause is in scope for the first and reading it there is a dead zone error rather than a read of the outer name. That scope is instantiated before the first case test is evaluated rather than when the first clause runs, so a case test can sit in the dead zone of a declaration three clauses below it. And a `default` written in the middle is still compared after every case, so it is a fallback in the comparison order and a position in the layout at the same time.

`break` and `continue` with nothing to leave are early errors rather than runtime ones, so scope analysis counts enclosing loops and enclosing breakables as it walks and refuses with node's exact messages. A `continue` lowers to a jump to the loop's back edge rather than to the loop's top, so an iteration that continues still passes through the instruction that counts iterations, and a loop that is mostly continues still gets hot when there is a tier to get hot for.

A clause not taken costs 9.2 ns and a switch that matches its first clause costs 11.0 ns, which is what a linear scan of `StrictEqual` and `JumpIfTrue` looks like and is the number a jump table would have to beat when there is a reason to build one.

### Objects are objects now

An object in the cage was a record until #49: a fixed set of names decided when it was made, with no prototype, no attributes, no way to delete a name and no way to add one. That was the right thing to build so that `console.log` could exist before there was an object model, and it was the wrong thing to keep, because a JavaScript object grows.

It is a transition tree now. A shape is a node, an edge is one property being added and the root is the empty object, so two objects built by adding the same names in the same order arrive at the same node without anything ever comparing two property lists. Insertion order is part of a shape's identity because the language says string keyed enumeration order is insertion order, and a tree gives that for free. An ordinary object is a sixteen byte header followed by its inline slots, with anything that does not fit going to an array on the side that starts at four slots and doubles.

Kind discrimination stopped needing a tag for the kind there is most of. The first word of every cage object is a slot, a small integer there is a kind tag and a pointer there is a shape, so an ordinary object is exactly the object whose first word is a pointer.

Then #50 added the literal, which is what makes any of it reachable from a program. It lowers to `new_object` and one `set_prop` per property, which is three instructions where one would do and is deliberate: a store is the operation that takes a transition, so a literal and an object grown a property at a time walk the same path and land on the same shape node. Neither needs a code path the other does not have, every property is already an inline cache site for the caches M1 still owes, and a duplicate property name comes out right without anything being written to make it.

Coercion came with it. ToPrimitive is one function with three callers that disagree about when it runs, in ways that are the language rather than an accident. `+` converts both sides and then asks whether either is a string, which is not the same as asking first and converting after, and `{} + 1` is the case that tells them apart because the answer is `[object Object]1` and neither operand was a string on the way in. The relational operators convert before the string test as well, which is why `'9' < {}` is true and is a comparison of code units. Loose equality treats two objects as the identity question and converts an object against a primitive before asking again.

### Two bugs the harness found and reading would not have

A literal writes its destination register before its operands run, because the stores need somewhere to store into. `x = {a: (x = 1)}` was already handled and `x = {a: x}` was not, so the property was handed the half made object instead of the value `x` was holding. Every other expression in the language produces its value before anything is written, which is why an object literal is the only place in the lowerer that has to ask whether an operand so much as reads a variable.

Node prints an object with nothing in it as `{}` however deep it is, because its empty check comes before its depth check. That reads as cosmetic and is not: `[Object]` is six characters longer than `{}`, and it was enough to break a line node keeps whole, so getting the order wrong changed the shape of the output two levels up.

Printing an object that can be reached from inside itself was built in the same pull request, with four rules that were measured against node rather than recalled. The test is whether the walk is currently inside the object and not whether it has ever seen it. The numbers are handed out where the way back is found rather than where the object was first printed, and they start again for each value `console.log` is given. The cycle check runs before the depth check. And the `<ref *1>` prefix counts towards the width arithmetic even though it sits outside the braces.

### Where the numbers stand

Reading the property added last is about one nanosecond at any property count and reading the first of sixteen is fifteen, which is what a linear parent chain walk looks like and is the number an inline cache exists to remove. A store that takes a transition another object already took is 3.2 ns against 37 for one that has to allocate a shape, so the tree is worth about eleven to one. A shape is 24 bytes against the 64 the spec budgeted and an empty object is 16 against 24.

Building an object, per object, with the fresh isolate each iteration needs subtracted rather than hidden inside the answer: an empty literal is 6.5 ns, two properties is 18.6 and four is 32.6, so a property costs about six nanoseconds. The same four property object built out of an empty literal and four separate stores is 41.9, so the room in `new_object` is worth about nine nanoseconds on an object that size.

The differential harness now generates objects, literals with duplicate names, property stores and property reads alongside everything it had before, and 5,006 programs agreed with node with no divergences.

The conformance number does not move, and the reason is the reason to keep reading it as a shape rather than a percentage. The suite's harness has a `try` on the line after the `switch` that used to block it, so 75,804 cases now stop one statement later than they did. Exceptions are next.

## 0.1.0

The first minor release, because M0 is done. Every box on the milestone is ticked: a parser, a bytecode compiler, an interpreter, a value representation, a heap, a command line, and now the two things that tell us whether any of it is right.

M0 was never about features. It is the skeleton the rest of the project hangs on, and the question it had to answer is whether the shape is one that can carry a JIT, an ahead of time compiler and a Node compatible surface without being torn up. Three pull requests since 0.0.6 turned that from an opinion into a measurement.

### Conformance has a number

The test262 runner is wired up and reports 6.65%, in #44. That is 5,405 cases of the 81,225 it attempted, and the number is small for a reason that is worth reading before drawing anything from it: 75,804 of the cases it could not run stop at the same `switch` statement in the suite's own `harness/assert.js`, so most of the suite is not being attempted rather than being failed. One missing statement form is standing in front of five sixths of the suite.

The expectations file is checked in, so a case that starts passing and a case that stops passing are both a diff rather than a number moving. That matters more than the percentage does, because the percentage is going to move in jumps as language features land and a jump tells you nothing about what quietly broke on the way.

### The differential harness, and the four bugs it found

The plan in the milestone was to run the interpreter against itself. That would have found nothing, because with one tier the only thing a comparison against yourself can catch is nondeterminism. So the harness in #45 uses node as the oracle instead, which is the thing we have to agree with anyway, and tier against tier goes in when there is a second tier in M6.

It generates seeded programs from a small grammar, runs them under both engines, and compares what came out. Three decisions do most of the work. A construct we have not implemented is a third verdict rather than a disagreement, so the untested count reads as a work list instead of a pile of bugs. Errors compare on the constructor name and not the message, because the standard says which error is thrown and says nothing about what it says. Two engines both refusing to parse is agreement, which was the surprise, because node's syntax error starts with the path of the temporary file it was handed and comparing that text reported a divergence on every program either engine rejected.

On its first thousand programs it found three real bugs, none of which any test in the repository was going to reach.

`undefined` was not a binding at all, and neither were `NaN` and `Infinity`. They are properties of the global object rather than keywords, and the reason nothing noticed is that `typeof` of a name nobody bound returns "undefined" rather than throwing, so the two ways of asking agreed for the wrong reason.

`console.log(-0)` printed `0`. The fix goes in the inspection path and deliberately not in the string coercion path, because `String(-0)` really is `"0"` and only the console is supposed to tell the two zeroes apart.

`v = 1.5 && ("x" + v)` answered `x1.5` instead of `x3`. Lowering passed the destination register down into an operand of a short circuiting operator, which overwrites the variable before the right hand side reads it. The module doc states the rule that forbids exactly this, three comments above the arm that broke it, and the compound assignment form one arm over was correct.

A longer run then found a fourth, in #46, and it is the one that makes the case for the whole exercise. `9007199254740993 / 10` printed `900719925474099.3` where node prints `900719925474099.2`. Both are the shortest form and both read back as the same double, which is exactly `900719925474099.25`, so the two candidates are equally far away. The standard says take the even one in that case and Rust takes the larger one. About five percent of doubles are in that family. Nothing about it is findable by reading, and a hand written test only catches it if whoever wrote the test already knew the rule, in which case they would have implemented it.

Two thousand generated programs run against node on every commit now, as a CI job of its own so a divergence is not buried in a test log. A divergence arrives shrunk to the statements that still reproduce it, with the seed to get it back.

### Where the numbers stand

Nothing in the performance picture changed this release, because nothing this release was about performance. Hello world still starts in 1.55 ms on `m4` against 24.63 ms for node, still in 2.44 MiB against 48.14 MiB, and those figures still mean what 0.0.6 said they mean, which is that we are ahead because we do less rather than because we are better.

What is different is that there is now a floor under the correctness side. Two thousand programs a commit and a checked in expectations file are the two things that make a performance number worth quoting later, because a fast engine that computes the wrong answer is not a data point.

### What is not here

No objects with shapes, no user defined functions, no closures, no modules, no event loop, no `process`. That is M1 and M2 and the honest reading of 0.1.0 is that it is a runtime in the sense that it runs a program, not in the sense that it runs your program.

## 0.0.6

Two pull requests on. The command line does what its help text says, which means you can point the binary at a file and it behaves the way a runtime is supposed to behave from the outside.

### The three commands

`katsu run`, `katsu check` and `katsu --build-info`, in #41. Running a file already worked once `console.log` landed, so most of this is the parts around it: which stream a message goes to, which code the process exits with, and a `katsu check` that runs a compiler rather than telling you to go and run one yourself.

The rule for messages is whether a Node process given the same file would have failed the same way. A program that throws prints its error and message bare on standard error and exits 1, because that is the line Node ends with and it is the line scripts grep for. A file that will not open, or a command that is not built yet, wears a `katsu:` prefix so nobody goes looking for a bug in their own source. There is no stack trace under the error yet, because a frame needs a source span and a function name and those arrive with the work that makes `x is not a function` name `x`.

`katsu check` looks for a compiler in `KATSU_TSC`, then in a `node_modules/.bin` walking up from the entry, then on the path, and names all three when there is none. The project one beats the global one on purpose: a project pins its TypeScript version in its `package.json` and its types only check against that version, so a globally installed compiler of a different version is a different answer rather than a substitute. A `tsconfig.json` at or above the entry means the project is checked rather than the single file. The compiler's exit code is forwarded and nothing of ours is printed over the top of it. Spec 4.2.1 writes the search down. Checked end to end against TypeScript 7.0.2.

### Two things that were wrong

`--build-info`, `--heap-census` and `--tier` were global flags, so `katsu run app.js --build-info` printed our build information instead of handing the flag to the program. A program could never be given a flag we happen to share a name with. They are top level flags now.

`--build-info` printed `target: aarch64`, which is half a target and useless in a bug report. It prints the architecture and the operating system now.

Arguments after the script are warned about rather than dropped, since they cannot be delivered until there is a `process` object in M2 and a program seeing an empty argument list with no explanation is a worse way to find that out.

The tests on that crate went from 3 to 25, and 13 of them run the real binary. Which stream something goes to and what code it exits with are exactly what a unit test cannot see and what a refactor can break silently.

### The first cold start number

This is the release where the cold start axis becomes measurable, so spec 15.5.1 records it. Hello world on `m4`, from the harness in `katsu-bench`, 25 runs with no warmup.

| Runtime | Version | Median | Peak RSS | Binary |
|---|---|---|---|---|
| katsu | 0.0.5 | 1.55 ms | 2.44 MiB | 1.7 MiB |
| bun | 1.4.0 | 4.97 ms | 10.92 MiB | 60.6 MiB |
| deno | 2.9.6 | 12.49 ms | 31.09 MiB | 118.9 MiB |
| node | 26.8.1 | 24.63 ms | 48.14 MiB | 139.0 MiB |

15.9 times faster to start than Node, 19.7 times less memory, an artifact 80 times smaller. Against Bun, which is the runtime that actually competes on this axis, 3.2 times and 4.5 times.

The spec says the uncomfortable half next to the table. We are ahead because we do less, not because we are better, and Node is carrying a module system, a process object, an event loop and a filesystem inside that 24.63 ms. The reason to record it now is the 4 MiB idle budget in spec 02.3: spending 2.44 MiB with nothing implemented means the whole Node compatible surface has to fit in the 1.5 MiB left, which is a more demanding statement than the budget looked like in the abstract.

### Also

The first version of that table, published in #41 and corrected in #42, was half a millisecond slow on every row, because it came from a throwaway script that timed a Python `subprocess.run` and charged every runtime for the Python interpreter's own spawning cost. No conclusion changed, which is why it is named in the spec rather than quietly fixed. The rule it leaves behind is that a figure we publish comes from `katsu-bench` or it does not get published.

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
