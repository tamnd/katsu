# Tools

Internal tooling. None of it is published to crates.io and none of it has a stability promise.

`test262-runner` runs the ECMAScript conformance suite and diffs the result against a checked in expectations file, which is how a regression gets named instead of showing up as a pass rate that moved a bit. `differential` runs the same program through every tier with the interpreter as the oracle, and is the harness that catches optimizer bugs.

Two things named in `spec/16-package-layout.md` live in sibling repositories instead, because both of them need to install and run Node.js, Bun and Deno, and neither belongs in the dependency graph of a runtime. The Node.js compatibility harness is [tamnd/katsu-compat](https://github.com/tamnd/katsu-compat). The cross runtime benchmarks are [tamnd/katsu-bench](https://github.com/tamnd/katsu-bench).
