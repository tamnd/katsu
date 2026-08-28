# Package layout, crates, and stability

## 16.1 The repository

One repository, one Cargo workspace, one version number across all crates. A multi crate workspace released in lockstep avoids the version matrix problem that eats projects with independently versioned internal crates, and the cost is that a patch to the parser bumps everything, which is a price worth paying.

```
katsu/
  crates/
    katsu/                 the CLI binary
    katsu-runtime/         the umbrella facade: what AOT output and embedders link
    katsu-api/             the embedding API, the public surface
    katsu-node/            the Node compatible layer
    katsu-builtins/        ECMAScript builtins
    katsu-aot/             the Rust emitter
    katsu-jit/             tiers 1 and 2
    katsu-loop/            event loop and I/O
    katsu-vm/              interpreter, object model, isolates
    katsu-gc/              collector interface and binding
    katsu-ir/              bytecode, blueprints, the opcode DSL
    katsu-parse/           frontend: oxc adapter, scopes, lowering
    katsu-platform/        OS specifics, W^X, mmap, signals
    katsu-macros/          #[katsu::export] and the derives
    katsu-stencils/        build time stencil generation and the shipped artifacts
  tools/
    test262-runner/        the conformance suite and the expectations ratchet
    differential/          interpreter against the JIT tiers, interpreter as oracle
  spec/                    these documents
  xtask/                   the architectural rules that CI enforces
```

Two things that would otherwise live in `tools/` are separate repositories instead. The Node compatibility harness is `tamnd/katsu-compat` and the cross runtime benchmarks are `tamnd/katsu-bench`. Both of them have to install and run Node, Bun and Deno to do their job, and neither belongs anywhere near the dependency graph of the runtime.

Dependency direction is strictly downward through the layer stack in document 03.2, enforced in CI by a check that fails on an upward edge. Every architectural rule in this specification that is not mechanically enforced will be violated within a year, so the ones that matter get a test.

## 16.2 The name

**`katsu` is free on crates.io**, so the top level crate is `katsu` with no suffix and no workaround, and the internal crates are prefixed `katsu-` as above. The GitHub repository is `tamnd/katsu` and the binary on disk is `katsu`, which is what users actually type.

The only conflict is on npm, where `katsu` is held by an abandoned "nodejs content management framework" at v0.1.0 with eight downloads in the last month. Since we are a Rust project, npm matters only if we ship an installer, and that is scoped to `@katsu/cli`. A transfer request for the dead package is worth one email at some point and is not worth blocking on.

Separately, [FyraLabs/katsu](https://github.com/FyraLabs/katsu) is an existing Rust project, an image builder for Ultramarine Linux. It is not on crates.io and it is in a different domain, so the two can coexist, but it is worth knowing before someone reports it as a surprise.

Document 00 lists `gyoza`, `ohagi` and `karaage` as alternates, all free on crates.io, with `ohagi` the only one also free on npm.

## 16.3 Stability tiers

Not every crate deserves the same promise, and saying which is which in advance prevents the situation where every internal refactor is a breaking change.

| Crate | Tier | Promise |
|---|---|---|
| `katsu` (the CLI) | Stable at 1.0 | CLI flags and behavior follow semver |
| `katsu-runtime` | Stable at 1.0 | AOT output links it, so its surface is pinned by every binary ever built |
| `katsu-api` | Stable at 1.0 | full semver, deprecation before removal, this is the contract for embedders |
| `katsu-macros` | Stable at 1.0 | the macro surface is part of `katsu-api`'s promise |
| `katsu-node` | Stable at 1.0 | tracks Node's own API stability |
| everything else | Internal | no promise, may change in any release |

The internal crates are published anyway, because they have to be for the workspace to build from crates.io and because a researcher wanting only our bytecode format or only our parser adapter should be able to have it. They carry a documented warning in their README rather than a false promise.

`katsu-api` being the only deeply stable programmatic surface is what makes the rest of the engine changeable. Document 03.7 puts `katsu-node` on top of `katsu-api` for exactly this reason: if the Node layer needs something the public API cannot express, that is a bug in the public API, discovered by us rather than by an embedder.

## 16.4 Feature flags

```toml
[features]
default   = ["jit", "node", "full-intl"]

jit       = []              # tiers 1 and 2; off gives an interpreter-only build
aot       = []              # the Rust emitter, only needed by the build tool
node      = []              # the whole Node compatibility layer
addons    = ["node"]        # Node-API host and dlopen; a security decision
full-intl = ["icu"]         # ECMA-402 through ICU4X
minimal   = []              # ECMA-429 Minimum Common API only, no Node layer
tokio-rt  = []              # own the tokio runtime rather than borrowing one
uring     = []              # io_uring backend, off by default per document 12.3
```

The combinations that must build and be tested, because they are real products rather than theoretical:

**Full** is the default and is what `katsu run` is. **Embedded**, meaning `jit` with `minimal` and no `node`, is the plugin engine use case from document 11.9 and it is the configuration a Rust application embeds. **Jitless**, meaning `node` without `jit`, is for platforms with no writable executable memory and for users who prefer the smaller attack surface per document 14.8. **Minimal jitless** is the smallest possible build and its size is a published number, because it is the number that decides whether we are usable on embedded targets.

A feature flag that is not in CI is a feature flag that is broken, so each of those four configurations builds and runs its applicable tests on every commit.

## 16.5 Versioning and the MSRV

Semver, with the tiers in 16.3 defining what "breaking" means for each crate.

The bytecode format has its own version number, independent of the crate version, embedded in every cache file and every snapshot. A mismatch means the cache is discarded and regenerated, silently and correctly, never loaded optimistically. Document 04.6 depends on this and it is the kind of thing that becomes a corruption bug if it is treated casually.

The Node-API version is separate again, defaulting to 8 per document 10.5.

MSRV is stable Rust, no more than six months behind current, bumped only in a minor release and noted in the changelog. **We do not require nightly.** Document 05.3 puts the `become` based interpreter behind a nightly feature flag precisely so that this stays true, and if `become` stabilizes around 2027 as the Trifecta Tech project goal targets, it becomes the default at the release after our MSRV reaches it.

## 16.6 Dependencies

We take the boring ones and we count them.

The load bearing external dependencies are `oxc` for parsing and possibly resolution, `regress` for regular expressions, `tokio` for the event loop, the chosen collector library from document 08.1, `icu4x` behind the `full-intl` feature, and possibly `cranelift` depending on the M6 decision in document 06.7. `ryu` and `lexical` for number formatting and parsing, because correct double to string is a research paper and not a thing to write by hand.

Total dependency count and total transitive count are published per release and treated as a budget, because a supply chain is an attack surface and because every dependency is a thing that can stop being maintained. `cargo-deny` runs in CI for licenses, duplicate versions and advisories.

Anything with a C or C++ build dependency needs a specific justification, since the moment we require a C toolchain to `cargo install` we have lost the deployment simplicity that document 09 sells. The one unavoidable exception is stencil generation in document 06.3, and that is why the stencils are pre-generated and shipped as build artifacts rather than generated on the user's machine.

## 16.7 Licensing

MIT or Apache-2.0 at the user's option, which is the Rust ecosystem default and the least friction for adoption in commercial settings.

The dependency licenses are checked mechanically. Node's own test suite and test262 have their own licenses, so they live in `tools/` as vendored or submoduled test data with their licenses intact and are not part of the distributed artifact.

## 16.8 Release artifacts

Per release, per tier 1 platform from document 14.11: a standalone binary, a container image built from `scratch` or `distroless`, the crates on crates.io, and the pre-generated stencil archives.

Installation should be a curl script, a Homebrew formula, a Cargo install, and a container pull, and none of them should require a compiler toolchain on the user's machine.

Binaries are signed, notarized on macOS with the JIT entitlement that document 06.9 requires under the Hardened Runtime, and published with checksums and a provenance attestation. Reproducible builds are a goal rather than a promise at 1.0, and the stencil determinism check in document 14.9 is the hardest part of getting there.
