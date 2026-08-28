# Changelog

Versions are cut on a fixed rhythm rather than when something feels finished. A patch release goes out every few merged pull requests so that there is always a recent tag to bisect against and to point a bug report at, and a minor release, 0.x.0, goes out when a milestone in the roadmap is done. Everything below 1.0 is a skeleton being filled in and nothing here is a stability promise.

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
