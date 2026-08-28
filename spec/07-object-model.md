# The object model

This is where a JavaScript engine's performance and its memory footprint are both decided, which makes it the document where the two halves of the 10x goal pull hardest against each other.

## 7.1 Values

Two representations, used in different places, which is the arrangement V8 arrived at and the one that satisfies both halves of our goal.

**In heap slots: 32 bit compressed values.** Object property slots, array elements, and context slots are 4 bytes. A slot is either a Smi (a 31 bit signed integer, tagged by the low bit) or a compressed pointer, which is a 32 bit offset from the cage base.

**In registers and on our stack: 64 bit tagged values.** Interpreter registers and JIT virtual registers hold a full 64 bit value, so doubles do not have to be boxed while they are in flight, and the tag scheme distinguishes Smi, double, and pointer without a memory access.

The reason to compress heap slots is that this is where the memory goal lives. V8 measured up to 43% heap reduction from pointer compression, up to 20% off Chrome's renderer memory, because tagged values are around 70% of a real heap. A V8 developer put the cost of turning it off at 60 to 70% more memory. There is no version of a 10x memory target that does not include this.

The reason not to compress registers is that a compressed double has to be boxed, and boxing every intermediate in a numeric loop is exactly the tax that makes an engine slow.

The consequence is that loading a value from a slot into a register is a decompress (add the cage base, or a shift plus add for a Smi) and storing is a compress with a check. Those are one or two instructions, they are what every V8 property access already pays, and they are the price of the memory goal.

### 7.1.1 The 64 bit register encoding, decided

This is the concrete scheme, implemented in `crates/katsu-vm/src/value.rs`. It is JavaScriptCore's, and the interesting part is why it is not the scheme everybody reaches for first.

The obvious NaN boxing scheme says that a double is anything that is not a NaN, and that every quiet NaN bit pattern is therefore free to carry a payload. That is wrong on the hardware we ship on. x86 SSE produces `0xFFF8_0000_0000_0000` as the result of an invalid operation, which is a *negative* quiet NaN, so the sign bit cannot be used to separate payloads from arithmetic results and the free space is not where the scheme assumes it is. Engines that started with the naive layout ended up carving exceptions into it. Offsetting the doubles avoids the question rather than answering it.

The encoding, on 64 bit platforms:

| Range | Meaning |
|---|---|
| `0x0000_0000_0000_0000` | empty, meaning no value at all, distinct from `undefined` |
| `0x0000_0000_0000_0002` | `null` |
| `0x0000_0000_0000_0006` | `false` |
| `0x0000_0000_0000_0007` | `true` |
| `0x0000_0000_0000_000A` | `undefined` |
| `0x0000_0000_0000_0000 ..= 0x0000_FFFF_FFFF_FFFF` | pointer, otherwise |
| `0x0002_0000_0000_0000 ..= 0xFFF2_0000_0000_0000` | double, with `2^49` added |
| `0xFFFE_0000_0000_0000 ..= 0xFFFE_FFFF_FFFF_FFFF` | 32 bit integer in the low half |

A double is encoded by adding `2^49` to its bit pattern and decoded by subtracting it. Every finite double, every infinity, and the one canonical NaN lands in a band that sits above every representable pointer and below the integer tag, so the three cases are separated by a single unsigned comparison each and no case needs a mask.

Two details that are load bearing. NaN is canonicalised to `0x7FF8_0000_0000_0000` before the offset is added, because without that the largest NaN bit pattern wraps past the integer tag and becomes an integer. JavaScript has exactly one observable NaN, so nothing is lost. And the immediates are laid out so the common predicates are one mask and one compare: `null` is `0b0010` and `undefined` is `0b1010`, differing in one bit, so the nullish test that `??` and `?.` need is a mask and a compare rather than two compares. `false` is `0b0110` and `true` is `0b0111` on the same principle, with the value itself in the low bit.

Integers get the top of the range rather than the bottom because a negative integer sign extends, and putting the tag above the sign extension is what lets the payload be read with a truncation instead of a mask and a shift.

`from_f64` chooses the integer form when the double is exactly an integer in range, which is what a bytecode does when it does not yet know the shape of a number. It deliberately keeps negative zero as a double, because `Object.is(-0, 0)` is false and collapsing it to the integer zero would be an observable bug.

The pointer range asserts rather than masks. Current 64 bit platforms give userspace 48 bits of address and the cage in 7.2 is a 4 GB reservation well inside that, so a real heap pointer always fits. If a future platform hands out wider addresses we want a panic on the first allocation, not silently truncated pointers.

Benchmarks are in `crates/katsu-vm/benches/value.rs` and they exist as a regression guard rather than as a published result, for the reason document 15 gives about microbenchmarks. They run on both reference machines from document 15.5, because an encoding whose whole justification is an x86 hardware quirk should not be measured only on ARM. Nanoseconds per value, over batches of 1024:

| Benchmark | Apple M4 | Core i9-13900K |
|---|---|---|
| encode `i32` | 0.26 | 0.28 |
| encode double | 0.27 | 0.38 |
| encode `f64` with the integer check | 0.52 | 0.94 |
| decode, numeric dispatch over a mixed batch | 0.46 | 0.27 |
| decode, `to_boolean` over a mixed batch | 0.55 | 1.01 |
| decode, the `is_i32` predicate | 0.06 | 0.17 |
| round trip, `i32` | 0.05 | 0.10 |
| round trip, double | 0.56 | 0.45 |

The number to take from this is not the speed, it is the shape. Everything here is well under a cycle per value on both machines, which means the compiler is vectorising the encode and decode work, which means the tagging did not introduce a serial dependency or a branch the predictor has to guess at. The day one of these rows becomes a whole cycle per value is the day something in the encoding started costing what it is not supposed to cost.

The two machines disagree in both directions and that is worth keeping rather than smoothing over. x86 is ahead on numeric dispatch and on the double round trip, ARM is ahead on the predicates and the integer round trip. Neither difference is about the encoding, both are about how the two vector units handle a 64 bit compare and select, and a benchmark that reported only the machine that flattered us would have hidden that.

## 7.2 The cage

All of the JavaScript heap lives in one reserved region of virtual address space per isolate, aligned so that the base can be held in a pinned register and the compression is a masked add.

This caps a single isolate's heap at 4 GB, which is exactly the trade Chrome made. It is a product limitation and it goes in the documentation rather than being discovered by a user with a 6 GB working set. `--no-pointer-compression` exists as an escape hatch for those users and it costs them the memory win and the sandbox.

The cage is also the security boundary. The threat model from document 01.7 is an attacker turning a JIT type confusion into a corrupted pointer and then into arbitrary process read and write. Inside a cage, a corrupted reference is an offset that lands somewhere else inside the cage, which is a much weaker primitive. Native pointers therefore do not live in the cage: an object that references host memory holds an index into an external pointer table, and the table lives outside. Cloudflare's Workers hardening describes the shape we are copying, including large unmapped guard regions around the cage so that out of bounds indexing faults rather than reads.

One honest note: because compressed pointers have no spare bits, the whole heap needs a single memory tag, which makes ARM MTE useless for catching corruption between objects inside the heap. That is a real loss and it is why the fuzzing in document 14 matters more for us than for a runtime that could rely on hardware tagging.

### 7.2.1 The cage and the slot encoding, decided

Implemented in `crates/katsu-gc/src/cage.rs` on top of the reservation primitive in `crates/katsu-platform/src/reservation.rs`.

The cage is a four gigabyte region reserved with a four gigabyte alignment, and the alignment is the whole trick. It forces the low thirty two bits of the base to zero, so decompressing an offset is a bitwise or with no carry and no masking, and compressing an address is a truncation. Above the cage sits another four gigabytes of reserved but permanently unmapped address space. Typed array index arithmetic is a thirty two bit offset added to a base inside the cage, so an out of bounds index can land at most four gigabytes past the end, and the guard turns that from a read of whatever the allocator put there into a fault.

The eight gigabyte reservation costs address space and nothing else. It is mapped `PROT_NONE` with `MAP_NORESERVE`, and pages are committed from the bottom as the heap grows. This distinction has to stay visible in every memory number the project publishes: the reserved figure is eight gigabytes on every process and is meaningless, the committed figure is what a container's memory limit counts and is what document 02.3's budget is measured against. The collector interface therefore reports committed bytes from `reserved_bytes`, with a comment saying why, because the alternative is a memory number that is off by three orders of magnitude and looks authoritative.

A heap slot is thirty two bits with the tag in bit zero:

| Bit 0 | The other thirty one bits |
|---|---|
| 0 | a thirty one bit signed integer, range −2^30 to 2^30 − 1 |
| 1 | a byte offset into the cage, with the low three bits zero because objects are eight byte aligned |

The tag is in the low bit rather than the high one so that decoding the integer is an arithmetic shift right, which brings the sign back for free. Objects are eight byte aligned, which leaves three spare low bits and means a real offset never collides with the tag.

One consequence is worth stating because things will depend on it: a slot of all zero bits is the integer zero. Freshly committed pages read as zero, so an object's uninitialised slots are already a valid number and nothing has to walk a new block to fill it in. That is a small win at allocation time and a large one at startup.

Narrowing a register value into a slot can fail, and `Value::to_slot` returns an option rather than making a decision on the caller's behalf. A double, or an integer outside the thirty one bit range, needs a heap number, which is an allocation and therefore not something a compression function should do quietly. `undefined`, `null`, `true` and `false` become pointers to singletons in the realm snapshot, which does not exist before M1. Both cases return nothing today and the caller has to handle them, which is better than either becoming a plausible looking wrong pointer.

Widening in the other direction cannot fail, because every thirty two bit pattern names either an integer or a byte inside the cage.

Measured per operation on both reference machines from document 15.5, over batches of 4096:

| Operation | m4 | gamingpc |
|---|---|---|
| decompress an offset to an address | 0.10 ns | 0.10 ns |
| compress an address back to an offset | 0.25 ns | 0.51 ns |
| integer round trip through a slot | 0.08 ns | 0.19 ns |
| bump allocate 24 bytes into committed pages | 2.4 ns | 2.1 ns |
| bump allocate 37 bytes, so the rounding is on the path | 2.3 ns | 2.1 ns |
| allocate when the pages have to be committed first | 194 ns | 243 ns |

The allocation figures include the census bookkeeping, because the census is on the allocation path rather than sampled and there is no path that skips it. The committed and uncommitted cases are reported separately on purpose, because their average describes neither of them. The last row is a syscall and belongs to the kernel more than to us, which is the honest reading of the gap between the two machines on that line.

The first run on gamingpc is also where the eight gigabyte reservation first failed, with ENOMEM out of criterion's warmup. The cause was the benchmark and not the cage: criterion's `SmallInput` batching runs the setup for a whole batch before timing any of it, so hundreds of heaps were alive at once and each one holds its own reservation. Linux refuses that and macOS does not, which is the entire argument for having a second reference machine.

## 7.3 Why not Nova's index based design

Nova represents every heap reference as a type discriminated 32 bit index into a per type vector, which gets pointer compression for free, prevents type confusion by construction because reinterpreting an index changes which arena you read, and lays out objects data oriented so a field read does not drag unused fields into cache. It is a genuinely interesting design and the Web Engines Hackfest slides make a good case.

We are not taking it, for one reason: an inline cache and a piece of JIT generated code want to load a property with a single memory access at a known offset from a known base. An index into an arena is one more indirection on the hottest path in the engine, and it is an indirection the JIT cannot optimize away because the arena can move and grow. Nova is interpreter only today, and this is the part of its design that a JIT would stress hardest.

If the copy and patch spike in M2 turns up something that changes this analysis, document 17 is where it gets revisited. Otherwise: tagged compressed pointers into a cage.

## 7.4 Shapes

Objects with the same property layout share a shape, and a shape describes the property names, their attributes, and their storage offsets. Adding a property transitions to a new shape via a transition table, so `{}` then `.x` then `.y` always reaches the same shape and ten thousand objects built the same way share one description. This is Chambers, Ungar and Lee's maps from SELF in 1989 and every engine since has some version of it.

Layout of an ordinary object:

```
[ shape (compressed) ] [ elements (compressed) ] [ properties (compressed) ] [ inline slot 0 .. N ]
```

Header is two words in compressed form. Inline slots hold the first N properties directly in the object, so a small object is one allocation. Overflow goes to the out of line properties array. Indexed properties go to elements.

Under the memory budget, N is chosen from the shape's transition history rather than being a fixed guess: an object literal with three properties allocates three inline slots, and a constructor that reliably adds five gets five once the shape tree has seen it happen. Over allocating inline slots is a classic way to burn memory on millions of small objects.

Dictionary mode exists for objects that are used as hash maps, with thousands of properties or frequent deletes. Transitioning into dictionary mode is one way, it disables inline caching for that object, and it exists so that pathological programs degrade instead of exploding the shape tree.

## 7.5 Inline caches, with one description per cache

The best documented modern design here is SpiderMonkey's CacheIR: rather than hand writing a stub for each cache case, an inline cache is a small program in a dedicated IR, and one description generates the interpreter's cache, the baseline stub, and the input the optimizing compiler uses for inlining decisions.

We copy that idea, because it is the same argument as document 05.2 applied to caches instead of opcodes. Writing property access fast paths three times, once per tier, is how tiers drift apart.

A cache program is a short sequence of operations: guard the shape, guard the prototype chain is unmodified via a validity cell, load from a fixed slot, or call a getter. The interpreter runs it as data. Tier 1 compiles it into an inline slab in the generated code (document 06.2). Tier 2 reads it as type information and inlines the load with a guard.

Cache states are the standard progression: uninitialized, monomorphic, polymorphic up to four shapes, then megamorphic, which falls back to a global stub cache keyed by shape and property name so that a genuinely polymorphic site is still better than a full lookup.

Prototype chain invalidation uses validity cells: a shape's cell is invalidated when anything on its prototype chain is mutated, so a cache does not need to walk the chain to know it is still valid.

## 7.6 Arrays

Arrays get specialized element storage, because a JavaScript array is not one data structure.

| Kind | Storage |
|---|---|
| PackedSmi | 4 byte compressed Smi values, no holes |
| PackedDouble | 8 byte unboxed doubles, no holes |
| PackedObject | 4 byte compressed references, no holes |
| Holey variants of each | same, with a hole sentinel |
| Dictionary | sparse, hash map |

Transitions only go one way, from more specific to less. Writing a double into a PackedSmi array converts the whole backing store once. Writing past the end creates holes and transitions to holey, and the holey kinds are meaningfully slower because every read has to check for the hole and consult the prototype chain if it finds one.

Typed arrays and `ArrayBuffer` are separate: the backing store is a raw allocation outside the cage, reached through the external pointer table, which is what makes zero copy sharing with Rust possible in document 11.

## 7.7 Strings, and the UTF-16 problem

JavaScript strings are sequences of UTF-16 code units and may contain lone surrogates. Rust's `String` is guaranteed UTF-8. There is no free conversion between them, and this is the single most consequential representation decision for a JavaScript engine written in Rust.

The design:

**Two flat representations.** Latin-1, one byte per character, covering the overwhelming majority of real strings, and UTF-16, two bytes per code unit, for everything else. A one byte string that never needs widening never pays double, which is where a lot of the memory win over a naive UTF-16 engine comes from.

**Ropes for concatenation.** `a + b` in a loop builds a tree rather than copying, and the tree is flattened lazily on the first operation that needs contiguous storage. This is what makes string building in JavaScript not quadratic.

**Slices** for `substring`, referencing the parent's storage, with the usual care that a small slice of a huge string keeps the huge string alive, so slices flatten when the ratio is bad.

**Interned atoms** for property names, in a table that lives in the realm snapshot, so `"length"` is one pointer comparison.

**The Rust boundary is explicit.** `Value::as_str()` does not exist as a free operation. Converting a JavaScript string to a Rust `&str` is either free (Latin-1 that is ASCII, or already valid UTF-8), a copy (UTF-16 to UTF-8), or an error (lone surrogates). Document 11 makes the API force that choice rather than hide it, because hiding it is how you get a runtime that silently copies megabytes on every call.

`regress` handles the regex side, since it offers UTF-16 and UCS-2 input modes where surrogate pairs split freely, which is what strict `RegExp` semantics require and what `regex` cannot do.

### 7.7.1 The flat string layout and the atom table, decided

Implemented in `crates/katsu-gc/src/string.rs` and `crates/katsu-gc/src/atom.rs`. Ropes and slices are not implemented yet, and the paragraph at the end of this section says what that costs.

A flat string is a three word header followed by the characters:

| Word | Contents |
|---|---|
| 0 | the shape slot, zero until M1 gives strings a map |
| 1 | length in UTF-16 code units |
| 2 | the hash in the top twenty eight bits, four flag bits below it |

The flag bits are two for the representation, which leaves room for ropes and slices, one for interned, and one saying whether the hash has been computed. Packing the hash and the flags into one word is what V8 does and it is what gets the header to twelve bytes rather than sixteen. That matters more than it sounds: it takes a ten character ASCII string from the 26 bytes budgeted in 7.9 down to 22 requested and 24 after alignment.

The representation is canonical. A string is stored as UTF-16 only when it actually contains a code unit above 255, and narrowing happens once, at construction, on every path in. Two things fall out of that. Equal strings always agree on their representation, so equality rejects on the header before it looks at a character, and the memory win over an engine that stores everything wide is taken by construction rather than by an optimisation pass that might not run.

The hash is computed lazily and cached in the header, and it is defined over code units rather than over the stored bytes. Defining it over code units is what lets the atom table hash a Rust `&str` and go looking for it without building a candidate string first, which is the difference between a property lookup that allocates and one that does not. It costs a code unit at a time rather than eight bytes at a time, which is the right trade for names and the wrong one for a megabyte of text used as a key.

The atom table is open addressed with linear probing at a three quarter load factor. An empty bucket is a slot of all zero bits, which is the integer zero by 7.2.1 and therefore never a pointer, so a freshly zeroed table is an empty table and growing one does not need a pass to write sentinels. The table's buckets are a Rust `Vec` outside the cage today, so the heap census does not see them, which is a hole in the accounting rather than a design. M1 moves the table into the realm snapshot where 7.7 says it belongs and where the census sees it for free.

Two gaps are worth naming. Without ropes, `a = a + b` in a loop copies both sides every time and is quadratic, which is the first thing M1 should close. And the hash is not resistant to flooding: a program that picks colliding property names can push the table into long probe runs, which is a real denial of service against a server. The fix is a per process random seed, and it has to arrive with the realm rather than later, because a seed cannot change once a hash has been cached in a string header.

Measured per operation on both reference machines from 15.5:

| Operation | m4 | gamingpc |
|---|---|---|
| build an eleven character ASCII string | 16.6 ns | |
| build an eleven code unit UTF-16 string | 31.7 ns | |
| compare two equal eleven character strings | 3.4 ns | |
| hash eleven characters of Rust text | 5.9 ns | |
| borrow a fifty nine character ASCII string as `&str` | 3.2 ns | |
| copy a twelve character Latin-1 string to UTF-8 | 38.3 ns | |
| atom lookup that hits, in a table of five hundred | 10.3 ns | |
| atom lookup that misses | 6.3 ns | |
| intern a name that is not in the table yet | 34.3 ns | |

The first line started at 37.6 ns. The obvious way to write the constructor narrows the text into a `Vec<u8>` and hands that to the Latin-1 path, which is a malloc and a free per string, and a parser builds one of these per identifier per file. Writing straight into the cage instead took it to 16.6 ns. The benchmark is the only reason anybody noticed.

The last two lines are the pair worth watching. A lookup that hits does a hash, a probe and a comparison and allocates nothing, which is the property every property access depends on.

## 7.8 Snapshot constraints

Because the realm is snapshotted at build time (document 03.8), nothing in the snapshotted heap may contain an absolute address.

Compressed slots are already offsets, so they survive relocation for free, which is a pleasant second reason to compress. Native function pointers are stored as ordinals into a table resolved at map time. External pointer table entries are rebuilt at startup. Anything else that needs an absolute address either does not go in the snapshot or gets a fixup entry, and there is a debug assertion that walks the snapshot looking for violations.

## 7.9 The per object memory budget

The numbers this design is aiming at, as targets that document 15 measures rather than claims already met:

| Thing | Bytes |
|---|---|
| Empty object `{}` | 24, being header plus a small inline slot allowance |
| `{a: 1, b: 2, c: 3}` | 36 |
| Array of 1000 packed Smis | ~4 KB plus header |
| Array of 1000 packed doubles | ~8 KB plus header |
| Short ASCII string, 10 chars | 26 budgeted, 22 measured, 24 after alignment (7.7.1) |
| Closure with two captured variables | 40 plus the shared blueprint |
| Shape, shared across all objects with that layout | ~64, amortized to near zero per object |

The comparison that matters is not against a hand written Rust struct, it is against V8, which already compresses. We win on header size, on not allocating feedback until a function is warm, and on sizing inline slots from real transition history. We do not win by 10x, and document 02.7 says so.
