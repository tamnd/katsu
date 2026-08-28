# Rust interop, both directions

This is half of why the project exists. A JavaScript runtime written in Rust that talks to Rust through the same foreign function ceremony everyone else uses has wasted its main structural advantage.

## 11.1 The bar we are measured against

**napi-rs** is the standard and it is genuinely good. It builds Node addons in Rust through Node-API with no `node-gyp` involvement, generates the JavaScript bindings and the type definitions from the Rust source, gets ABI stability across Node versions for free from Node-API, handles cross compilation, and since v3 can also target WebAssembly through an external Node-API polyfill at essentially zero cost. The crate is at 3.10 as of July 2026 and the CLI at 3.4. Anything we build has to be at least this pleasant or nobody will use it.

**deno_core** is the standard for the other direction, embedding JavaScript in a Rust program. Its `#[op2]` macro is designed to cross the boundary as efficiently as possible, using V8's Fast API where it can, and it maps promises onto Rust futures. Its own documented weaknesses are instructive: thin documentation, and a layer that gives you an engine without a runtime, so `console.log` does not work out of the box and there is no `fetch`. `rustyscript` exists as a third party layer specifically to make deno_core pleasant, which tells you where the gap is.

Two conclusions. First, both directions are solved problems ergonomically, so there is no excuse for shipping something worse. Second, everyone else pays an FFI boundary that we do not have to.

## 11.2 The structural advantage

napi-rs addons are shared libraries loaded with `dlopen`, communicating through a C ABI, converting every value through `napi_value` handles, one call at a time. That boundary exists because Node is C++ and your code is Rust and they meet at a versioned ABI.

For us, an extension is a Rust crate, and the runtime is a Rust crate, and they are compiled together by the same compiler. There is no ABI. There is no `dlopen`. A `&str` argument can be passed as a `&str`. `rustc` can inline across the boundary. In AOT mode, where the user's JavaScript has also become Rust in the same crate graph, a call from JavaScript into a Rust function can compile to a direct call with no marshalling at all when the types line up.

That is the thing we can do that nobody else can, and it is worth building the API around.

We still support the boundary version, because the ecosystem lives there: napi-rs addons and any other Node-API addon load through the host in document 10.5, unmodified. Static linking is the fast path, not the only path.

## 11.3 Rust called from JavaScript

```rust
use katsu::prelude::*;

#[katsu::export]
pub fn parse_config(source: &str) -> Result<Config, ConfigError> {
    Config::from_toml(source)
}

#[katsu::export(name = "hashFile")]
pub async fn hash_file(path: PathBuf) -> Result<String> {
    let bytes = tokio::fs::read(path).await?;
    Ok(hex::encode(blake3::hash(&bytes).as_bytes()))
}

#[katsu::export]
pub struct Database {
    inner: sled::Db,
}

#[katsu::export]
impl Database {
    #[katsu::constructor]
    pub fn open(path: PathBuf) -> Result<Self> { ... }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> { ... }
}
```

From JavaScript:

```js
import { parseConfig, hashFile, Database } from "./native";

const config = parseConfig(await readFile("app.toml", "utf8"));
const db = new Database("./data");
const hash = await hashFile("./big.bin");
```

The macro generates the argument conversion, the type checks, the error translation, and a `.d.ts` file, in the way napi-rs already established. `snake_case` becomes `camelCase` unless overridden, because a Rust API that looks foreign in JavaScript will be wrapped by hand anyway.

The type mapping:

| Rust | JavaScript | Cost |
|---|---|---|
| `i8`..`i32`, `u8`..`u32`, `f64` | number | free |
| `i64`, `u64`, `i128`, `u128` | BigInt | allocation |
| `bool` | boolean | free |
| `&str`, `String` | string | see 11.5 |
| `&[u8]`, `Vec<u8>` | Uint8Array | zero copy for `&[u8]`, move for `Vec<u8>` |
| `Vec<T>`, arrays | Array | element wise |
| `HashMap<String, T>` | object | element wise |
| `Option<T>` | `T` or null | free |
| `Result<T, E>` | `T` or a thrown error | free on the ok path |
| `#[derive(ToJs, FromJs)]` struct | object with a fixed shape | one shape, precomputed |
| `#[katsu::export]` struct | class instance holding the Rust value | one allocation |
| `impl Future` | Promise | see 11.7 |
| `impl Iterator` | iterable | lazy |
| `External<T>` | opaque handle | free |

`#[derive(FromJs)]` on a struct precomputes the shape, so decoding an object with known fields is a shape comparison and a fixed offset load per field rather than a property lookup per field. This is a real speedup over generic conversion and it is only possible because we own the object model.

## 11.4 JavaScript called from Rust

```rust
use katsu::{Runtime, Realm, Value};

let rt = Runtime::new()?;
let realm = rt.new_realm()?;

realm.global().set("answer", 42)?;

let result: i32 = realm.eval("answer * 2")?;

let module = realm.import("./app.js").await?;
let handler: Function = module.get("handler")?;
let response: Response = handler.call_async((request,)).await?;
```

Handles and scopes come from document 08.4 and the borrow checker enforces them, which is the thing rusty_v8 has to enforce by documentation. `Value` is a handle, not a pointer, and it cannot outlive its scope because the compiler says so.

The design targets the gap that deno_core's own documentation admits: this API comes with a runtime. `console.log` works. `fetch` works. Modules resolve. The Node layer is a feature flag away, so an embedder who wants a sandbox with no host access gets one, and an embedder who wants to run an existing npm package gets that too, with the same crate.

### 11.4.1 Where what a program prints goes, as built

The first half of that promise is real now, and the shape it took is worth writing down because it is the shape every host facility will take. `console.log` does not call `println!`. It calls a sink the isolate owns, and an embedder can replace it.

Three sinks ship. `Standard` writes to the process's own streams and is what an isolate nobody has changed has. `Recorder` keeps everything written to it and hands it back, which is how a test asserts on what a program printed and how an embedder puts a script's output in its own log. `Discard` throws it away, which is what a host running untrusted code wants. Replacing one returns the one that was there, so a caller can capture output for one call and put the old sink back.

The sink is on the isolate rather than on the process for the same reason everything else in 03 is: two isolates on two threads printing into one buffer is exactly the arrangement this design exists to rule out, and an embedder that runs a script wants that script's output rather than every script's output. It is one sink and two streams rather than two sinks, because `console.log` goes to standard output and `console.error` goes to standard error and a program that redirects one and not the other depends on that, while a recorder wants both in the order they were written.

A write that fails is dropped. Node turns a closed pipe into an `EPIPE` and exits, which needs an exit code and a process object to hang it on, and neither exists in M0. That is written down rather than left to be discovered by somebody piping katsu into `head`.

## 11.5 Ownership across the boundary

The rules, stated plainly because every subtle interop bug lives here.

**Rust values passed to JavaScript are moved or cloned, never borrowed**, unless the borrow is bounded by a scope the API can see. JavaScript can retain a reference for as long as it likes and no Rust lifetime can describe that, so `#[katsu::export]` structs are moved into a JavaScript object that owns them, and the collector drops the Rust value when the object dies.

**Strings are the sharp edge.** Document 07.7 explains why: JavaScript strings are UTF-16 and may contain lone surrogates, Rust strings are UTF-8, and there is no free conversion in general. `&str` as a parameter is free when the JavaScript string is Latin-1 ASCII, which is the common case, a copy when it is UTF-16, and an error on a lone surrogate. The API makes the choice visible: `&str` fails on unpaired surrogates, `Utf16String` accepts anything, and `Cow<str>` lets a caller see whether a copy happened. Hiding this is how you ship a runtime that silently copies megabytes per request.

**Buffers are zero copy in both directions and that is a hazard.** `&[u8]` from a `Uint8Array` borrows the backing store for the duration of the call, and the JavaScript side must not detach or resize the buffer while Rust holds it. Detaching during a synchronous call is impossible because JavaScript is not running, but it is very possible across an await point, so async functions taking buffers either copy or take an owned `Bytes` that holds the backing store alive. Resizable `ArrayBuffer` and `SharedArrayBuffer` get the same treatment. This is the one place where an interop mistake becomes memory unsafety, so it is the one place the API is deliberately less convenient.

**`External<T>`** wraps an arbitrary Rust value as an opaque JavaScript value with no methods, for handles that pass through JavaScript without being touched. It costs one external pointer table entry.

## 11.6 Errors

A Rust `Result::Err` becomes a thrown JavaScript error, with the `Display` output as the message and the error type name preserved. A user type implementing `katsu::JsError` controls the constructed error class and its properties, so a Rust error enum can surface as a proper `TypeError` or a domain error class with a `code` field.

Going the other way, a thrown JavaScript exception becomes `Err(katsu::Error::Js(value))`, carrying the thrown value rather than a stringification, because half of real JavaScript code throws objects.

A Rust panic across the boundary is caught, converted into a JavaScript error, and reported with the panic message and a note that it was a panic. It does not unwind through generated code and it does not abort the process by default, because a plugin panicking should not kill a server. `--panic=abort` exists for embedders who want the strict behavior.

## 11.7 Async

A Rust `Future` returned to JavaScript becomes a Promise, resolved when the future completes. A JavaScript Promise awaited from Rust becomes a Future, and `.await` on it drives the JavaScript event loop as needed.

The runtime integrates with tokio rather than reimplementing an executor, which document 12 details. `Runtime::new()` takes a handle to an existing tokio runtime if there is one, so embedding katsu inside an existing tokio application does not spawn a second thread pool.

Blocking Rust work called from JavaScript is the classic footgun, since it stalls the loop exactly as blocking JavaScript would. `#[katsu::export(blocking)]` moves the call onto the blocking pool and returns a Promise, which makes the right thing the easy thing.

## 11.8 Threads

An isolate is `Send` but not `Sync`, per document 03.6. Only its owning thread may touch its values, which is why `Value` is neither `Send` nor `Sync` and the compiler enforces it.

To call into JavaScript from another thread you send a closure to the isolate's queue:

```rust
let handle = realm.thread_handle();   // Send + Sync
std::thread::spawn(move || {
    handle.call(|realm| {
        realm.global().get::<Function>("onEvent")?.call((payload,))
    });
});
```

This is the same mechanism as `napi_threadsafe_function` in document 10.5, exposed as a Rust API rather than a C one, and there is exactly one implementation underneath.

## 11.9 What good looks like

Document 15 measures these, and they are targets rather than achievements:

Calling an exported Rust function with two numbers from JavaScript should cost single digit nanoseconds in AOT mode, where it is a direct call, and tens of nanoseconds in JIT mode. napi-rs pays a C ABI transition and handle setup on every call and is the comparison point.

Passing a 1 MB `Uint8Array` to Rust and back should be zero allocations and zero copies.

Decoding a ten field object into a Rust struct through `#[derive(FromJs)]` should be within a small factor of `serde_json` deserializing the equivalent, since it does strictly less work.

Embedding the runtime in a Rust binary should add a small number of megabytes and a start time under a millisecond, which is the number that makes katsu usable as a plugin engine inside other Rust programs. That use case is not an afterthought, it is a product, and document 16 gives it a crate with its own stability guarantee.
