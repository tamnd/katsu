# Security

## Reporting

Report vulnerabilities through [GitHub private security advisories](https://github.com/tamnd/katsu/security/advisories/new) rather than a public issue.

katsu is pre 1.0 and has no users, so there is no embargo policy worth writing yet and no bounty. What there is instead is a commitment that a report gets read and answered.

## What is in scope, eventually

A JavaScript engine's threat model is that it runs code it does not trust. The CVE history of the field is overwhelmingly memory corruption in two places, the JIT and the object model, and that is precisely the code that motivated writing this in Rust rather than C++.

The places where Rust does not help on its own, and where the review effort therefore goes, are the ones `spec/14-quality-bar.md` names: the unsafe code in the object model and the collector, the correctness of the machine code the JIT emits, the write xor execute discipline over executable pages, the heap cage that pointer compression depends on, and the Node-API host, which by definition loads native code that we did not write and cannot verify.

Native addon loading is off by default for that last reason. Turning it on is a decision the person deploying makes, not one we make for them.

## What is not in scope

That a program can consume all available memory or all available CPU. A runtime executes the program it is given.

That the JIT is present at all. If a writable executable page is unacceptable in your environment, build with `--no-default-features --features node`, which is the jitless configuration, and it is a supported product rather than a degraded one.
