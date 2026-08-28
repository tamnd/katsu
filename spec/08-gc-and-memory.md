# Garbage collection and the memory budget

## 8.1 We are not writing a collector

A production garbage collector is a multi year project and there are now two Rust-friendly libraries built precisely so that language implementers do not have to repeat it.

**MMTk** is the Rust framework: allocators, spaces and work packets composed into plans, with officially supported bindings for JikesRVM, OpenJDK and Ruby, and third party ones for GHC, PyPy and Scala Native. Ruby 3.4 shipped modular GC with MMTk as an option. It is Rust, which makes it the default candidate.

**Whippet** is Andy Wingo's collector library, described in "Nofl: A Precise Immix" (arXiv 2503.16971, March 2025). It is a no dependency library meant to be embedded in the host's source tree, offering a serial semi space collector, a parallel copying collector, the Nofl based mostly marking collector, and a Boehm shim, chosen at compile time. Nofl pushes Immix's reclamation granularity down to the allocator's minimum alignment, fixing the Immix worst case where one small object pins two 128 byte lines, and reports outperforming copying and mark sweep collectors at tight to adequate heap sizes. Tight heap performance is exactly our situation, given document 02's budget. It is C, so it costs an FFI layer.

One correction to a thing that would be easy to assume: **LXR is not available in mmtk-core.** The reference counting plus Immix collector from PLDI 2022 that reports better throughput and pauses than production collectors lives on a branch of a fork, not in the released library. It is a research option, not a dependency.

**The decision is deferred to a measurement at M4**, and document 03.7 already isolates katsu-gc so that switching is a contained change. What is decided now is the interface, not the implementation.

## 8.2 The interface the collector sees

The collector knows nothing about JavaScript. It sees:

- **Allocation**: a size, an alignment, and a space (young, old, large object, code, external).
- **Object scanning**: given an object, enumerate its outgoing references. Driven by the shape, since the shape already describes the layout, so scanning an ordinary object is walking its slot count.
- **Root scanning**: enumerate the roots. Our own stack (document 05.4) makes this precise and cheap, a walk down a contiguous region reading known slots from known frame layouts, with no conservative scanning and no register guessing.
- **Write barriers**: the fast path is open coded by us into the interpreter and both JIT tiers, because a barrier that costs a function call is a barrier that ruins the mutator.
- **Safepoints**: our interrupt flag check at back edges and function entry (document 5.6) is the safepoint mechanism.

Whippet explicitly exposes enough detail for the embedder to open code allocation fast paths, safepoints and write barriers, which is exactly what this interface needs. MMTk exposes the same shape through its binding API. Writing our own on top of either is not the risk; the risk is that one of them imposes a barrier or scanning convention that costs us more than the other, which is what M4 measures.

## 8.3 The plan

Generational Immix, or Sticky Immix, as the starting plan.

JavaScript's allocation profile is the textbook generational one: enormous numbers of very short lived objects (arguments objects, iterator results, closures, boxed doubles, intermediate strings) with a small fraction surviving. Immix gives bump allocation into 32 KB blocks made of 128 byte lines with opportunistic evacuation to control fragmentation, which is most of a copying collector's allocation speed without paying a semispace.

Fragmentation is our specific worry rather than a general one, because document 02.7 already concedes that heap size under load is an axis where we do not win by much, and a fragmenting collector would turn "not much" into "worse". That is the argument for evaluating Nofl seriously: it exists to fix precisely that weakness in Immix.

Concurrent marking is not in scope before 1.0. Pause times matter, but a correct stop the world generational collector with short young generation pauses is the right first thing, and adding concurrency to a collector that works is a known path.

## 8.4 Handles and rooting

Any moving collector requires that the runtime never hold a raw pointer across a point where collection can happen. The Rust way to enforce that is a handle scope with a lifetime:

```rust
fn some_builtin(scope: &mut HandleScope, args: Args) -> Result<Local<Value>> {
    let obj: Local<Object> = args.get(0).to_object(scope)?;
    let key = scope.atom("length");
    let len = obj.get(scope, key)?;      // may allocate, may collect, may move obj
    Ok(len)                              // obj is still valid: it is a handle, not a pointer
}
```

`Local<T>` borrows from the scope, so the borrow checker refuses to let it outlive the scope, and the scope's slots are scanned as roots. This is the same idea as V8's `Local` and `HandleScope`, except that in C++ it is a convention enforced by review and in Rust it is enforced by the compiler. It is one of the two or three places where writing this in Rust produces a genuinely better engine rather than an equivalent one.

Persistent handles exist for references that outlive a scope, they are registered in a root table, and they are explicitly dropped. Weak persistent handles are the machinery behind `WeakRef` and `FinalizationRegistry`.

Builtins never see a raw pointer. That is what makes the `#![forbid(unsafe_code)]` rule on katsu-builtins from document 03.2 possible at all.

## 8.5 Weakness and finalization

`WeakMap` and `WeakSet` need ephemeron semantics: a value is reachable only if its key is reachable, which requires a fixpoint during marking rather than a single pass. This is a known algorithm and both candidate libraries need to support it, so it is an evaluation criterion at M4 rather than something we bolt on later.

`WeakRef` and `FinalizationRegistry` need the collector to report deaths, and the specification requires that finalization callbacks run on the event loop rather than during collection, so document 12's loop grows a phase for them.

Native objects held by Node-API addons are the awkward case: an addon holds a reference to a JavaScript object and expects a finalizer when it dies. That path has to work correctly or native modules leak, and it is on the M8 test list in document 13.

## 8.6 Off heap memory

`ArrayBuffer` backing stores, `Buffer` data, compiled code buffers, and anything shared with Rust live outside the cage and outside the collector's heap, reached through the external pointer table.

They still count against the process. So the memory manager tracks external bytes and includes them in the pressure signal that triggers collection, because the classic embarrassing bug is a process holding two gigabytes of `Buffer` with a 4 MB JavaScript heap and a collector that sees no reason to run.

Code buffers are a special case with their own budget and their own eviction, specified in document 06.8.

## 8.7 Making the budget real

Document 02.3 sets a 4 MB idle target with a line item budget. A budget that is not a test is a wish, so:

**A CI test measures RSS** for a set of fixed programs on a fixed machine image and fails on regression beyond a threshold. Hello world, an empty HTTP server, a program that imports twenty common packages, and a program that allocates and drops ten million objects.

**A heap census command**, `katsu --heap-census`, prints the line items from the 02.3 table for a running process, so that when the number regresses the next question is answerable in one command rather than a day of profiling.

**Heap growth is conservative by default.** The heap grows on a ratio of live data with a cap, and a `--max-heap` flag hard limits it. Serverless and container users care about the ceiling far more than about the last few percent of throughput.

**Memory is returned to the operating system.** After a collection that frees a meaningful fraction, unused blocks are released with `MADV_DONTNEED` or the platform equivalent. A runtime that grows to 200 MB during startup and never gives it back has failed the memory goal regardless of what its heap accounting says, and this is a common enough sin that it deserves its own test.

## 8.8 Where this axis will embarrass us

Honesty, since document 02 promised it.

Our first collector will be worse than V8's under adversarial allocation patterns. V8 has had fifteen years of tuning on the specific shapes real JavaScript produces, plus concurrent marking, plus a young generation sized by heuristics learned from the entire web.

The mitigations are that we start from a research collector rather than an amateur one, that the interface lets us swap plans on the strength of a measurement, and that our budget discipline means we are defending a much smaller heap in the first place. The failure we are most likely to hit is fragmentation under long running server workloads, and the earliest place we would see it is the soak test in document 14, which is why that test exists from M5 rather than from 1.0.
