//! Benchmarks for the tagged value encoding.
//!
//! These are microbenchmarks, and katsu-bench says in as many words that microbenchmarks of
//! individual operations are not published as results. That still holds. This file exists as a
//! regression guard, not as a source of numbers for anybody's blog post. Boxing and unboxing sits
//! underneath every single bytecode the interpreter will ever run, so if it gets slower we want to
//! find out on the pull request that made it slower rather than three months later in JetStream.
//!
//! Everything here is measured over a batch rather than a single call, because a single encode is
//! a couple of instructions and the timer costs more than the thing being timed. Batching moves
//! the measurement above the noise floor and has the useful side effect of measuring the shape the
//! interpreter actually sees, which is a run of values going through the same path.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use katsu_vm::Value;

const BATCH: i32 = 1024;

/// A spread of integers across the whole 32 bit range rather than a run of small ones, so the
/// benchmark does not accidentally measure a branch predictor that has learned the input. The
/// stride is prime and the arithmetic wraps on purpose, which is the cheapest way to get a walk
/// that visits every part of the range without sorting anything.
fn integers() -> Vec<i32> {
    (0..BATCH)
        .map(|i| i32::MIN.wrapping_add(i.wrapping_mul(4_194_301)))
        .collect()
}

/// Doubles that are not integers, so `from_f64` takes the boxing path rather than the integer
/// shortcut. The integer shortcut is measured separately and conflating the two would hide a
/// regression in either.
fn doubles() -> Vec<f64> {
    (0..BATCH).map(|i| f64::from(i) * 1.5 + 0.25).collect()
}

/// The mix an interpreter actually runs on. Real programs are mostly small integers with doubles
/// and objects appearing often enough that a benchmark of pure integers tells you nothing about
/// how the type dispatch behaves.
fn mixed() -> Vec<Value> {
    (0..BATCH)
        .map(|i| match i % 8 {
            0..=4 => Value::from_i32(i),
            5 => Value::from_double(f64::from(i) + 0.5),
            6 => Value::from_pointer(0x1_0000 + u64::from(i.cast_unsigned()) * 16),
            _ => {
                if i % 16 == 7 {
                    Value::UNDEFINED
                } else {
                    Value::TRUE
                }
            }
        })
        .collect()
}

fn encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");

    let ints = integers();
    group.bench_function("i32", |b| {
        b.iter(|| {
            let mut sink = Value::EMPTY;
            for &n in black_box(&ints) {
                sink = black_box(Value::from_i32(n));
            }
            sink
        });
    });

    let floats = doubles();
    group.bench_function("double", |b| {
        b.iter(|| {
            let mut sink = Value::EMPTY;
            for &n in black_box(&floats) {
                sink = black_box(Value::from_double(n));
            }
            sink
        });
    });

    // from_f64 is the one a bytecode calls when it does not know the shape of the number yet, so
    // it pays for the "is this exactly an integer" check on every value. This measures that toll.
    group.bench_function("f64_with_integer_check", |b| {
        b.iter(|| {
            let mut sink = Value::EMPTY;
            for &n in black_box(&floats) {
                sink = black_box(Value::from_f64(n));
            }
            sink
        });
    });

    group.finish();
}

fn decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");

    let values = mixed();

    // The add fast path in miniature: check for two integers, fall through to double, give up on
    // anything else. If tagging ever gets in the way of this shape, this is where it shows.
    group.bench_function("numeric_dispatch", |b| {
        b.iter(|| {
            let mut total = 0.0f64;
            for &v in black_box(&values) {
                if let Some(n) = v.as_i32() {
                    total += f64::from(n);
                } else if let Some(n) = v.as_double() {
                    total += n;
                }
            }
            black_box(total)
        });
    });

    // What a conditional jump does to whatever is on top of the stack.
    group.bench_function("to_boolean", |b| {
        b.iter(|| {
            let mut taken = 0usize;
            for &v in black_box(&values) {
                if v.to_boolean() {
                    taken += 1;
                }
            }
            black_box(taken)
        });
    });

    // The predicates are meant to be a mask and a compare each. Measuring them next to the
    // accessors above tells us whether the Option is costing anything the optimiser cannot see
    // through, which is the thing most likely to quietly regress.
    group.bench_function("is_i32", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for &v in black_box(&values) {
                count += usize::from(v.is_i32());
            }
            black_box(count)
        });
    });

    group.finish();
}

fn round_trip(c: &mut Criterion) {
    let ints = integers();
    let floats = doubles();

    // Encode then immediately decode, which is what a register spill and reload costs. The
    // compiler is allowed to see through this and it should: the interesting result would be a
    // day when it cannot.
    c.bench_function("round_trip/i32", |b| {
        b.iter(|| {
            let mut total = 0i64;
            for &n in black_box(&ints) {
                total =
                    total.wrapping_add(i64::from(Value::from_i32(n).as_i32().unwrap_or_default()));
            }
            black_box(total)
        });
    });

    c.bench_function("round_trip/double", |b| {
        b.iter(|| {
            let mut total = 0.0f64;
            for &n in black_box(&floats) {
                total += Value::from_double(n).as_double().unwrap_or_default();
            }
            black_box(total)
        });
    });
}

criterion_group!(benches, encode, decode, round_trip);
criterion_main!(benches);
