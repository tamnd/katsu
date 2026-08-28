# Node compatibility

## 10.1 What the goal actually commits us to

"Runs 100% of TypeScript and JavaScript programs compatible with Node.js without modification" is the hardest sentence in this specification, and it is worth being precise about what it means before designing for it.

It means the language is complete, which document 14 measures against test262. It means the module system behaves exactly like Node's, which document 04.3 covers. It means the `node:` module surface is present and behaves the same. It means native addons load. And it means the failure mode when we fall short is a clear error naming the missing thing, not a wrong answer three hours into a production run.

The bar the field has set: Bun 1.2 reports 98% npm compatibility and runs Node's own test suite to measure it, but native `.node` addons still require a Node process. Deno 2.8 finished the compatibility work begun in 2.0, and Deno 3 added native addon support through `.node` files, with community reports of roughly 90 to 95% npm compatibility before that. Those percentages come from blog coverage rather than official measurement and should be read as directional. What is not directional is the shape of the remaining gap, and every source agrees on it: **native addons and packages reaching into Node internals are where compatibility dies.**

So this document spends most of its length on the part everyone else finds hardest.

## 10.2 The layer

`katsu-node` implements the Node surface in Rust, on top of `katsu-api`, with no unsafe code. It is a consumer of the public embedding API rather than a privileged insider, which is the constraint from document 03.7 that keeps the embedding API honest.

Builtin module source is not JavaScript that we parse at startup. Modules are Rust, registered in the realm, and captured in the build time snapshot from document 03.8, so importing `node:fs` costs a table lookup rather than a parse. Node itself pays real startup time for its JavaScript-implemented internals, and not paying it is a chunk of the cold start axis.

## 10.3 Globals and the web surface

`process` with `argv`, `env`, `platform`, `version`, `versions`, `exit`, `cwd`, `hrtime.bigint`, `nextTick`, `memoryUsage`, `on` for the signal and exception events, and `stdout`/`stderr`/`stdin` as real streams. `Buffer` as a `Uint8Array` subclass with the whole legacy method set, backed by memory outside the cage. `console` with the full Node formatting behavior including `%s`/`%d`/`%o`, `table`, `group`, `time`, and `dir` depth handling, because a surprising amount of debugging depends on the exact output. Timers with Node's ordering guarantees, `setImmediate`, and `queueMicrotask`. `__dirname`, `__filename`, `require`, `module` and `exports` in CommonJS scope; `import.meta.url`, `import.meta.dirname`, `import.meta.filename` and `import.meta.resolve` in ESM.

The web platform surface is the WinterTC Minimum Common API, standardized as ECMA-429: `fetch`, `Request`, `Response`, `Headers`, `URL`, `URLSearchParams`, `TextEncoder`, `TextDecoder`, `AbortController`, `AbortSignal`, `Event`, `EventTarget`, `structuredClone`, `crypto` and `crypto.subtle`, `performance`, `atob`, `btoa`, `Blob`, `File`, `FormData`, `WebSocket`, and the whole Streams set. This is the surface serverless platforms converged on and it is a standard we can conform to rather than a moving target we chase.

## 10.4 The module table

| Tier | Meaning | Modules |
|---|---|---|
| Exact | bug for bug, tested against Node's suite | `fs`, `path`, `buffer`, `stream`, `events`, `util`, `os`, `url`, `querystring`, `string_decoder`, `timers`, `assert`, `process`, `crypto`, `zlib`, `net`, `http`, `https`, `http2`, `tls`, `dns`, `child_process`, `worker_threads`, `readline`, `console` |
| Functional | correct behavior, internals differ | `vm`, `perf_hooks`, `async_hooks`, `diagnostics_channel`, `cluster`, `test`, `module`, `dgram`, `tty`, `repl`, `sqlite` |
| Shimmed | present, honest about what it is | `v8` returns plausible numbers, `inspector` supports a subset of the protocol, `trace_events` collects into our own tracing |
| Refused | throws with an explanation and a link | `process.binding`, `node:internal/*`, anything relying on a V8 internal representation |

The tiering exists because pretending is worse than refusing. A `v8.serialize` that produces something structurally different from V8's format will corrupt somebody's cache silently, so `v8.serialize` produces our own format with our own magic bytes and `v8.deserialize` rejects V8's, loudly.

`async_hooks` deserves a note because it constrains the whole runtime. `AsyncLocalStorage` is used by essentially every APM vendor and every modern logging setup, and supporting it means context propagation is designed into the event loop and the promise implementation from the start rather than retrofitted. Document 12 carries that requirement.

## 10.5 Native addons

This is the crux, and it is worth stating the conclusion first: **we implement Node-API natively, on our own object model, in the same process.**

Node-API is explicitly designed to be independent of the underlying JavaScript engine, and the Node-API team maintains documentation on binding it to engines other than V8. That is the entire reason it exists. So a correct `napi_*` implementation over our object model is a large but well specified piece of work rather than a research problem, and it is the single highest leverage compatibility investment in the project because it is what unlocks `better-sqlite3`, `sharp`, `bcrypt`, `canvas`, and the long tail of packages that make real applications work.

The mapping is mostly natural, which is a good sign:

`napi_value` is a handle from document 08.4, and `napi_handle_scope` is our `HandleScope`. That correspondence exists because both designs came from the same problem, and it means addon values are precisely rooted for free.

`napi_ref` with a refcount of zero is a weak reference, which needs the collector to report deaths and to run finalizers on the event loop rather than during collection. Document 08.5 already carries that requirement; this is what it is for.

`napi_create_external_arraybuffer` hands an addon a pointer to memory it owns, and the JavaScript side sees a normal `ArrayBuffer`. The external pointer table from document 07.2 is the mechanism, and it is the same one document 11 uses for Rust interop, which is a pleasant collapse of two problems into one.

`napi_threadsafe_function` lets a background thread schedule work onto the loop. It maps onto the loop's cross thread queue in document 12, and it must be correct under our thread per core isolate model where the addon's thread is not the isolate's thread.

The hard parts, named so they are not surprises: finalizer ordering at process exit, addons that assume `napi_value` is a pointer and cache it across scopes, the exact exception propagation semantics of `napi_is_exception_pending`, and `node-gyp` producing a build that links against our headers rather than Node's.

Loading an addon means `dlopen` on a shared library that runs arbitrary native code, so an addon can crash the process and can defeat every safety property in this document. That is true of Node too. It is documented, it is disableable with `--no-addons`, and it does not get papered over.

**Node-API version 8 is the default target.** The versioning model changed at version 9: versions are no longer strictly additive, so an addon written for 9 may need source changes for 10, and Node itself keeps compatibility by defaulting to the version 8 API surface unless an addon opts in. We do the same. We support 8 through the current version, we default to 8, and we honor `NAPI_VERSION` opt in.

## 10.6 The raw V8 API, and why we say no

A minority of addons skip Node-API and use V8's C++ API directly. Those cannot work on an engine that is not V8, because they depend on V8's class layout, its `Local`/`Isolate`/`Context` types, and its inline functions compiled into the addon.

Bun demonstrated that a shim is not impossible, building enough of the V8 API on top of JavaScriptCore to run the addons its users needed most. So "impossible" is the wrong word; "expensive, fragile, and version coupled" is the right one.

Our position for 1.0: no V8 API. Loading such an addon produces an error that names the package, explains why, and links to the tracking issue. Before 1.0 we measure which packages in the top ten thousand actually need it, and if the list is short and important, a targeted shim becomes a funded piece of work with real evidence behind it rather than a guess.

## 10.7 Divergences we will document rather than hide

Every one of these is a place where a real program can observe that it is not on Node, and each gets a documentation entry rather than a silent difference.

Error message text will not match Node's character for character everywhere, and some tests assert on it. We match the `code` property exactly, since that is what correct code checks, and we match message text for the errors Node's own test suite pins.

Stack traces are our format. `Error.captureStackTrace` and `Error.prepareStackTrace` with the `CallSite` object are V8 specific APIs that half the ecosystem's error libraries use, so we implement them, but stack frame contents will differ across inlining and tier boundaries. Getting this close enough that source map libraries work is an explicit M8 goal.

Timing and ordering that Node does not specify may differ, and code depending on unspecified ordering was already broken. Where Node does specify ordering, particularly microtask draining relative to loop phases, we match exactly, and document 12 treats that as a hard requirement.

`--inspect` speaks the Chrome DevTools Protocol subset needed for breakpoints, stepping, scope inspection, the console, and heap snapshots. Full protocol parity is not a 1.0 goal.

## 10.8 How we know

We run Node's own test suite. That is what Bun does, it is the only measurement anyone respects, and any other number is marketing.

The published compatibility number is pass rate on Node's suite by module, in a table, updated per release, with the failures listed. Document 14 specifies the harness and the reporting.

Beyond that, a corpus of real packages installed and exercised, weighted by npm download counts, because passing the standard library tests and failing to run Express are different kinds of success.

## 10.9 The order we build it

Dependency order from real applications, not alphabetical. `events`, `buffer`, `util`, `stream`, `path`, `process` and `fs` come first because everything else stands on them. Then `net`, `http`, `crypto` and `zlib`, which is enough to run a web server. Then `worker_threads`, `child_process`, `dns`, `tls` and `http2`. Node-API lands at M8 with its own milestone rather than being squeezed in, because it is the thing that decides whether the compatibility claim is real.
