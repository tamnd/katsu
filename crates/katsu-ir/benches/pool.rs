//! Benchmarks for the constant pool.
//!
//! Same caveat as the value benchmarks: this is a microbenchmark and katsu-bench does not publish
//! microbenchmarks as results. It exists as a regression guard on a data structure the lowering
//! pass hits once per literal and once per property name in the whole program, which is often
//! enough that a bad hash or an extra copy would show up as a frontend regression with no obvious
//! cause.
//!
//! Two shapes, because they measure different things. Adding names that are all new is the cost of
//! the pool growing, which is what a file full of distinct string literals pays. Adding names that
//! are already there is the cost of the lookup alone, which is what real code pays, because a
//! program that reads `.length` reads it in twenty places.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use katsu_ir::ConstantPool;

const BATCH: usize = 512;

/// Identifier shaped names rather than random strings, because that is what a constant pool holds
/// and because short strings with a shared prefix are the case a hash function can be bad at.
fn names() -> Vec<String> {
    (0..BATCH)
        .map(|index| format!("property_{index}"))
        .collect()
}

fn pool(c: &mut Criterion) {
    let names = names();

    c.bench_function("pool/distinct_strings", |b| {
        b.iter(|| {
            let mut pool = ConstantPool::default();
            for name in &names {
                black_box(pool.string(name));
            }
            black_box(pool.len())
        });
    });

    c.bench_function("pool/repeated_strings", |b| {
        let mut pool = ConstantPool::default();
        for name in &names {
            pool.string(name);
        }
        b.iter(|| {
            for name in &names {
                black_box(pool.string(name));
            }
        });
    });

    c.bench_function("pool/numbers", |b| {
        b.iter(|| {
            let mut pool = ConstantPool::default();
            for index in 0..u32::try_from(BATCH).expect("batch fits") {
                black_box(pool.number(f64::from(index) * 1.5));
            }
            black_box(pool.len())
        });
    });
}

criterion_group!(benches, pool);
criterion_main!(benches);
