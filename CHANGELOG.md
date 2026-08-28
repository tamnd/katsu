# Changelog

Versions are cut on a fixed rhythm rather than when something feels finished. A patch release goes out every few merged pull requests so that there is always a recent tag to bisect against and to point a bug report at, and a minor release, 0.x.0, goes out when a milestone in the roadmap is done. Everything below 1.0 is a skeleton being filled in and nothing here is a stability promise.

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
