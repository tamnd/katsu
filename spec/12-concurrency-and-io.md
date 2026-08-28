# The event loop, I/O, and concurrency

## 12.1 The event loop is a compatibility surface, not an implementation detail

Programs depend on Node's loop ordering. Not because they should, but because they do: a `setTimeout(fn, 0)` that runs before an I/O callback in Node and after it in katsu is a bug report that takes a day to diagnose and destroys trust. So the loop's observable behavior is specified as compatibility surface, and the implementation underneath is ours to choose.

## 12.2 Phases and microtasks

Node's loop runs phases in a fixed order, and we match it:

| Phase | What runs |
|---|---|
| Timers | `setTimeout` and `setInterval` callbacks whose deadline has passed |
| Pending callbacks | deferred system callbacks, mostly error paths |
| Poll | I/O completions, and blocking here when there is nothing else to do |
| Check | `setImmediate` callbacks |
| Close | `close` event handlers |

The rules that programs actually observe, and which are therefore tests:

The microtask queue drains completely between every callback, not between phases. A promise chain resolved inside a timer callback runs before the next timer callback.

`process.nextTick` has its own queue which drains before the microtask queue, and it drains fully, including ticks enqueued by ticks. This is a Node invention with no standard behind it and it is load bearing in enormous amounts of existing code.

`setTimeout(fn, 0)` against `setImmediate(fn)` at the top level has genuinely nondeterministic relative order in Node, and inside an I/O callback `setImmediate` always wins. We reproduce both, including the nondeterminism, because code that depends on the deterministic case is correct and code that depends on the other case was already broken.

Timers use a hierarchical timing wheel rather than a heap, since a server with a hundred thousand connections each carrying a timeout is a normal situation and per timer heap operations are not free. Node's 1ms minimum clamp and its `TimeoutOverflowWarning` behavior are reproduced.

## 12.3 The I/O backend

We use tokio, not our own reactor and not libuv.

tokio is the most heavily deployed async runtime in Rust, it is the ecosystem's schelling point so that every HTTP, TLS, and database crate works with it, and document 11.7 needs an embedder's existing tokio runtime to be reusable anyway. Writing our own reactor would buy us nothing and cost us the ecosystem.

Platform backends: epoll on Linux, kqueue on macOS and the BSDs, IOCP on Windows. This is what tokio already does and it is the boring correct answer.

**io_uring is an optional accelerator, off by default, and never a requirement.** This deserves the space because it is the decision most likely to be second guessed.

The case for it is real: with epoll, a tuned TCP proxy can spend 70 to 80% of its CPU cycles outside userspace on syscalls and data copying, and io_uring's submission and completion queue model removes much of that.

The case against is stronger for a runtime that has to run everywhere. **Docker 25.0.0 and later block `io_uring_setup`, `io_uring_enter` and `io_uring_register` in the default seccomp profile**, reversing an earlier policy that allowed them, motivated by Google security research showing io_uring vulnerabilities usable to escape containers. Docker Desktop 4.42.0 lists the same block. The practical result is that software requiring io_uring with no fallback simply fails to start in a default container: TigerBeetle fails with `PermissionDenied` and the mod.io SDK reports the same. On the Rust side, `tokio-uring` describes itself as young, has drawn community comment about sparse releases and a changelog untouched since 2022, and native io_uring support is expected in tokio 2.x rather than being available now. And per discussion with tokio's maintainers reported in February 2026, io_uring helps most for batching filesystem operations while real world networking gains are often limited.

So: `--io-backend=uring` opts in, capability is detected at startup, and any failure falls back to epoll with a warning rather than an abort. The place we expect it to actually pay is file I/O, which is where 12.4 needs it.

## 12.4 Files, and the thread pool we would like to delete

Node performs file I/O on a thread pool of four threads by default, sized by `UV_THREADPOOL_SIZE`, because POSIX has no good asynchronous file API. This is a well known bottleneck for file heavy workloads and it surprises people constantly.

We use tokio's blocking pool for the same reason and with the same limitation, sized by default to the core count rather than to four, and configurable. When io_uring is enabled, file operations go through it and become genuinely asynchronous with no thread pool involvement at all, which is the clearest win available from that backend and the reason the flag exists.

`fs.readFileSync` and its siblings stay synchronous, because their semantics are observable and startup code depends on them.

## 12.5 Isolates, threads, and workers

One isolate per thread, owning its heap, its code region, its loop, and everything in it. An isolate is `Send` and not `Sync`, so it can move between threads but never be touched by two at once, and the compiler enforces it. Document 03.6 sets this up and this is where it pays.

`worker_threads` maps directly: a `Worker` is a new isolate on a new thread with its own loop. Message passing is `structuredClone` semantics with transferables moving `ArrayBuffer` backing stores by pointer rather than by copy, which is what makes worker pipelines viable.

`SharedArrayBuffer` and `Atomics` are the only shared mutable memory, backed by memory outside every cage, and `Atomics.wait` and `Atomics.notify` are real futexes. Everything else is either copied or transferred.

The thread per core option, where an HTTP server runs N isolates each accepting on a shared listener with `SO_REUSEPORT`, is a deployment mode rather than a language feature. It is how you saturate a machine without the shared state problems that make multithreaded runtimes hard, it is the model tokio-uring itself adopts, and it is the reason document 02.5 believes in a 2 to 4x server throughput number.

`cluster` is implemented on top of workers rather than on child processes where possible, which makes it dramatically cheaper than Node's version while keeping the API.

## 12.6 Context propagation

`AsyncLocalStorage` is used by every APM vendor, every distributed tracing setup, and most structured logging. It requires that an asynchronous context be carried across every continuation: promise reactions, timers, I/O callbacks, `nextTick`, and worker message handlers.

This cannot be retrofitted cheaply, because it means every scheduling point has to save and restore a context pointer. So it is designed in from the beginning: the loop and the promise implementation carry a current context word, entering a callback swaps it, and leaving restores it. The cost is a couple of loads and stores per callback, which is acceptable, and the alternative is being unusable in production observability stacks.

`async_hooks` itself is the older, heavier API. We implement enough of it to support `AsyncLocalStorage` and the resource tracking that existing tools depend on, and we are explicit in document 10.4 that its internals differ.

## 12.7 Promises, rejections, and the parts that bite

Promises are implemented in the runtime, not in JavaScript, so a resolved promise does not allocate a JavaScript closure per reaction.

`unhandledRejection` and `rejectionHandled` follow Node's semantics exactly, including the detection being deferred to the end of the microtask checkpoint, because a rejection handled synchronously after creation must not warn. Node's default of terminating on unhandled rejection is matched, along with `--unhandled-rejections` and its modes.

Async stack traces are stitched across await points, since a stack trace that stops at the first `await` is nearly useless for debugging a server. This costs a captured frame per await on the slow path, and it is off in AOT release builds unless `--async-stack-traces` is passed.

## 12.8 Shutdown

`process.exit` is immediate and does not flush, matching Node, including the well known lost stdout on a pipe.

Natural exit happens when the loop has no pending work, and `beforeExit` can add more. `FinalizationRegistry` callbacks are drained at the point document 08.5 specifies. Signal handlers run as loop callbacks rather than in a signal context, so JavaScript never runs on a signal stack.

Graceful shutdown for servers, where the listener closes and in flight requests drain, is a documented pattern rather than a runtime feature, because that is where Node put it and code exists that does it by hand.

## 12.9 Why this is 2 to 4x and not 10x

Document 02.5 promises server throughput of two to four times Node's, and this is the document that has to justify not promising more.

Where the wins come from: no JavaScript to C++ boundary crossing on every I/O callback, since our runtime and our engine are the same Rust program; fewer copies on the path from the socket to a JavaScript `Buffer`, because we control both ends; a real thread per core mode instead of `cluster` with process overhead; file operations that can be genuinely asynchronous rather than pooled; and a promise implementation that does not allocate as much.

Where the ceiling is: a server that spends most of its time in `read`, `write`, TLS, and the kernel network stack is bounded by things a JavaScript engine cannot change. Node already uses epoll and kqueue, its HTTP parser is already C, and its TLS is already OpenSSL. The JavaScript engine is simply not the bottleneck in most real servers, which is precisely why the honest answer on this axis is a small multiple rather than the headline number, and why document 02 puts it in the column marked no.
