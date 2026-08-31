# Contributing

katsu is pre M0. The workspace, the layer stack and the tooling exist, the engine does not. That means the useful contributions right now are narrow, and it is worth saying which ones they are rather than letting people find out by having a pull request sit.

## What is useful right now

Answers to the open questions in [`spec/17-open-questions.md`](spec/17-open-questions.md), especially the ones that are a scripting job rather than a research project. Q11 asks which of the top ten thousand npm packages with native components link against V8 symbols directly instead of Node-API, and that is a weekend of work that changes how M8 gets scoped. Q8 asks what fraction of operations in fifty popular TypeScript packages our inference could prove, and it decides how much ahead of time work is worth doing.

Corrections to the specification. It is eighteen documents of claims about other people's systems, and some of them are wrong. A correction with a source is more valuable than a feature.

Tooling, test harnesses, and anything in [`tools/`](tools/).

## What is not useful yet

Large engine features. The order in [`spec/13-milestones.md`](spec/13-milestones.md) exists because the pieces are deeply coupled, and a garbage collector contributed before the object model exists is a garbage collector that gets thrown away.

## Before you open a pull request

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo run -p xtask -- layers
```

If you touched anything on a hot path, also run the benchmarks on a reference machine with `cargo run -p xtask -- bench --machine gamingpc -p <crate>`, and put the before and after in the pull request. A benchmark taken on your own laptop with a browser open is not evidence and reviewers will say so.

CI runs all four and a few more. The layer check is the one people hit first: dependencies point strictly downward through the stack in [`spec/03-architecture.md`](spec/03-architecture.md), and adding an upward edge fails the build with the edge named.

## Releasing

Bump `workspace.package.version` in the root `Cargo.toml` and the internal dependency versions under `[workspace.dependencies]` in the same edit, because they are the same number and CI fails if they drift. Run `cargo run -p xtask -- release` to check the workspace against what crates.io will accept, write the changelog entry, merge, then tag `vX.Y.Z` and push the tag.

The tag is the whole trigger. The release workflow builds a binary for every tier 1 platform, attaches them to a GitHub release with checksums and a provenance attestation, and publishes all fifteen crates to crates.io in one `cargo publish --workspace`. The publish runs the same check again with the tag name in it, so a tag that does not match the version in the tree fails before anything is uploaded.

Nothing here is undoable. A version on crates.io can be yanked but never replaced, so the checks run on every commit rather than at tag time, and the answer to a bad release is another release.

## House rules for code

No unsafe code above the platform, object model and JIT layers. `spec/14-quality-bar.md` is specific about where unsafe is allowed and what it costs to add a block, and the short version is that every one needs a comment saying what invariant makes it sound.

Errors are values. `todo!` and `unimplemented!` abort the process, which is a bad experience for someone who hit an unfinished path, so unfinished paths return an error that names the milestone instead.

Doc comments explain why, and link the specification document that made the decision. The what is usually visible from the signature.

## House rules for prose

Plain English. Say the thing. If a claim has a number, cite where the number came from, and if the number is ours, say what machine it was measured on.

Losses get written down with the same prominence as wins. That rule is in `spec/02-the-10x-goal.md` and `spec/15-benchmarks.md` and it applies to commit messages and issue comments too.
