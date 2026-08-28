//! Benchmarks for flat strings and for the atom table.
//!
//! Same standing as the other microbenchmarks in this crate: `spec/15-benchmarks.md` says these
//! are regression guards and not published results.
//!
//! Three things are worth guarding here. Interning is on the path of every property access that
//! misses a cache, so a lookup that hits has to be a hash and a comparison rather than an
//! allocation. The Rust boundary has a free path and a copying path, and the gap between them is
//! the whole argument for making the caller pick, so it should be visible in a number. And string
//! construction is what a parser does to every identifier and every literal in a file.
//!
//! Anything that builds a `BumpHeap` per iteration uses `BatchSize::PerIteration`, for the reason
//! written at the top of `heap.rs`: each heap holds its own eight gigabyte reservation and a
//! batched setup keeps hundreds of them alive at once.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use katsu_gc::{AtomTable, BumpHeap, StringRef, hash_str};

/// A short name, the length most property names actually are.
const NAME: &str = "constructor";
/// A sentence, for the paths where the length is what is being measured.
const SENTENCE: &str = "the quick brown fox jumps over the lazy dog and keeps going";

fn construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("string");

    // Building one identifier, which is what the parser does thousands of times per file. The
    // heap is pre-committed in the setup so this measures the narrowing check and the copy rather
    // than an mmap.
    group.bench_function("from_str_ascii_11", |b| {
        b.iter_batched(
            || {
                let mut heap = BumpHeap::new().unwrap();
                heap.reserve(1 << 20).unwrap();
                heap
            },
            |mut heap| {
                for _ in 0..256 {
                    black_box(StringRef::from_str(&mut heap, black_box(NAME)));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // The same length in code units, but wide enough that narrowing has to fail and the string
    // is stored two bytes per unit. The gap between this and the line above is what the canonical
    // representation costs on the way in.
    group.bench_function("from_str_utf16_11", |b| {
        b.iter_batched(
            || {
                let mut heap = BumpHeap::new().unwrap();
                heap.reserve(1 << 20).unwrap();
                heap
            },
            |mut heap| {
                for _ in 0..256 {
                    black_box(StringRef::from_str(
                        &mut heap,
                        black_box("日本語日本語日本語日本"),
                    ));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    let mut heap = BumpHeap::new().unwrap();
    let name = StringRef::from_str(&mut heap, NAME).unwrap();
    let same = StringRef::from_str(&mut heap, NAME).unwrap();
    let sentence = StringRef::from_str(&mut heap, SENTENCE).unwrap();
    let accented = StringRef::from_str(&mut heap, "café société").unwrap();
    let cage = heap.cage();

    // Equality between two strings that are equal, which is the case that has to walk every byte.
    group.bench_function("equals_11", |b| {
        b.iter(|| black_box(name).equals(cage, black_box(same)));
    });

    // The hash of text that has not been through the heap, which is the atom table's lookup path.
    group.bench_function("hash_str_11", |b| b.iter(|| hash_str(black_box(NAME))));

    // The free direction: ASCII in the cage is already UTF-8, so this is a bounds check and a
    // borrow.
    group.bench_function("to_utf8_ascii_59", |b| {
        b.iter(|| black_box(sentence).to_utf8(cage).map(|text| text.len()));
    });

    // The copying direction: Latin-1 above ASCII is one to two bytes per character, so this
    // allocates. Reported next to the line above because the gap is the reason the API makes the
    // caller choose.
    group.bench_function("to_utf8_latin1_12", |b| {
        b.iter(|| black_box(accented).to_utf8(cage).map(|text| text.len()));
    });

    group.finish();
}

fn interning(c: &mut Criterion) {
    let mut group = c.benchmark_group("atom");

    // A table with a realistic number of names in it, so the probe is not always on an empty
    // table. Five hundred is roughly what a small module's identifiers come to.
    let mut heap = BumpHeap::new().unwrap();
    let mut table = AtomTable::new();
    for i in 0..500 {
        table.intern(&mut heap, &format!("identifier{i}")).unwrap();
    }
    table.intern(&mut heap, NAME).unwrap();
    let cage = heap.cage();

    // The hot one. Every property access that misses an inline cache ends up here, and it must
    // not allocate.
    group.bench_function("lookup_hit", |b| {
        b.iter(|| black_box(&table).lookup(cage, black_box(NAME)));
    });

    // A miss that has to walk the probe run before it can say no.
    group.bench_function("lookup_miss", |b| {
        b.iter(|| black_box(&table).lookup(cage, black_box("constructo")));
    });

    // Interning something new, which allocates the canonical string and may grow the table. This
    // is the parser's path, and the growth is deliberately inside the measurement because a
    // rehash is part of what interning costs. The names themselves are built once, outside the
    // timing, so this is not a benchmark of `format!`.
    let fresh: Vec<String> = (0..256).map(|i| format!("name{i}")).collect();
    group.bench_function("intern_new_256", |b| {
        b.iter_batched(
            || {
                let mut heap = BumpHeap::new().unwrap();
                heap.reserve(1 << 20).unwrap();
                (heap, AtomTable::new())
            },
            |(mut heap, mut table)| {
                for name in &fresh {
                    black_box(table.intern(&mut heap, name));
                }
                (heap, table)
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(benches, construction, interning);
criterion_main!(benches);
