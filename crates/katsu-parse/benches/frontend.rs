//! Benchmarks for the front of the frontend: parse plus adapt, end to end.
//!
//! Same standing as the microbenchmarks in `katsu-gc`: `spec/15-benchmarks.md` says these are
//! regression guards and not published results.
//!
//! This measures `parse`, which is oxc's parser and our adapter back to back, and it does not try
//! to separate the two. That is deliberate rather than lazy. The adapter is `pub(crate)` because
//! the whole argument in `spec/04-frontend.md` is that exactly one module sees an oxc type, and
//! poking a hole in that to get a prettier benchmark would give up the property the benchmark
//! exists to protect. The number that matters for the startup budget is the total anyway, since a
//! program pays for both or neither.
//!
//! Two shapes are measured because they stress different things. A file of small functions is what
//! real code looks like and what the per node cost shows up in. A single long function is the
//! adapter's recursion depth and vector growth with no function boundaries to break it up.
//!
//! Throughput is reported in bytes, so criterion prints a figure in megabytes per second that can
//! be compared against the startup budget directly: a runtime that has to be running user code in
//! single digit milliseconds cannot spend most of that here.

use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::LazyLock;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use katsu_parse::parse;

/// A file of small functions, roughly the shape of a module a person would write.
///
/// Built once and reused, because the cost of building it is not what is being measured. Every
/// construct in here is in the M0 subset on purpose, so the benchmark measures the adapter doing
/// work rather than the adapter refusing on the first line.
static MANY_FUNCTIONS: LazyLock<String> = LazyLock::new(|| {
    let mut source = String::new();
    for index in 0..200 {
        let _ = write!(
            source,
            "function compute{index}(a, b) {{\n  const scaled = a * {index} + b;\n  if (scaled > 100) {{\n    return scaled - 100;\n  }}\n  return scaled;\n}}\n"
        );
    }
    source
});

/// One long function, for the path where nothing resets the recursion.
static ONE_LONG_FUNCTION: LazyLock<String> = LazyLock::new(|| {
    let mut source = String::from("function big(input) {\n  let total = 0;\n");
    for index in 0..600 {
        let _ = writeln!(source, "  total = total + input * {index};");
    }
    source.push_str("  return total;\n}\n");
    source
});

/// The same module written in TypeScript, so the erasure path is measured and not assumed.
static TYPED_MODULE: LazyLock<String> = LazyLock::new(|| {
    let mut source = String::new();
    for index in 0..200 {
        let _ = write!(
            source,
            "interface Input{index} {{ a: number; b: number }}\nfunction compute{index}(a: number, b: number): number {{\n  const scaled: number = (a * {index} + b) as number;\n  if (scaled > 100) {{\n    return scaled - 100;\n  }}\n  return scaled;\n}}\n"
        );
    }
    source
});

fn frontend(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");

    for (name, path, source) in [
        ("many_functions", "many.js", &*MANY_FUNCTIONS),
        ("one_long_function", "long.js", &*ONE_LONG_FUNCTION),
        ("typed_module", "typed.ts", &*TYPED_MODULE),
    ] {
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let module = parse(black_box(path), black_box(source)).expect("should parse");
                // Returned rather than dropped inside the closure so the tree's construction
                // cannot be optimised away, and so the drop is timed too. Freeing the tree is a
                // real cost of owning it, and hiding that would make the number a lie.
                module
            });
        });
    }

    group.finish();
}

criterion_group!(benches, frontend);
criterion_main!(benches);
