//! Benchmarks for the JavaScript stack.
//!
//! Same standing as the other microbenchmarks in this workspace: `spec/15-benchmarks.md` says these
//! are regression guards and not published results.
//!
//! Two things are worth measuring here and one of them is a claim the code makes about itself.
//!
//! Pushing and popping a frame is what every call pays, twice, and it is the number that says
//! whether the split between the value region and the frame header vector was affordable. The module
//! doc in `src/stack.rs` argues that the split is worth a second allocation and a second cache line
//! per call because it makes root scanning a slice walk. That argument is only honest if the cost is
//! known, so it is measured here rather than asserted there.
//!
//! Reading and writing a register is what every instruction pays. It is a bounds check against the
//! current frame and an indexed load, and the thing to watch is that it stays that and does not
//! quietly grow a branch.
//!
//! Frames of eight slots, because that is roughly what lowering produces for the small functions in
//! the frontend benchmarks, and a depth of a hundred, because a real call stack is deep enough that
//! the frame vector does not fit in the same cache line as its own length.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use katsu_ir::Register;
use katsu_vm::{Invocation, Stack, Value};

/// The frame size a small function lowers to.
const SMALL_FRAME: u16 = 8;

/// How deep to go, so that the frame vector is not a single cache line.
const DEPTH: usize = 100;

/// A call of three arguments into a function that declares three, which is the shape most calls in
/// real code have and the one where every argument is copied.
const THREE: Invocation = Invocation {
    arity: 3,
    first: Register(0),
    passed: 3,
    function: 0,
    return_pc: 0,
    return_to: Register(0),
};

fn calls(c: &mut Criterion) {
    let mut group = c.benchmark_group("stack");

    group.throughput(Throughput::Elements(1));
    group.bench_function("push_pop", |b| {
        let mut stack = Stack::new().expect("should reserve");
        b.iter(|| {
            stack
                .push(black_box(SMALL_FRAME), &[], 0)
                .expect("should have room");
            black_box(stack.pop());
        });
    });

    // What every call in a running program actually costs, which is the number the goal in spec 02
    // cares about. The arguments come out of the caller's own registers and never leave the region,
    // so the difference between this and the one above is the copy and the return bookkeeping.
    group.bench_function("call_and_return", |b| {
        let mut stack = Stack::new().expect("should reserve");
        stack.push(SMALL_FRAME, &[], 0).expect("should have room");
        b.iter(|| {
            stack
                .push_call(black_box(SMALL_FRAME), black_box(THREE))
                .expect("should have room");
            black_box(stack.pop());
        });
    });

    // The same call with the arguments arriving from outside the region, which is what an embedder
    // calling in does. Kept separate because it is a different path and a rarer one.
    let args = [Value::from_i32(1), Value::from_i32(2), Value::from_i32(3)];
    group.bench_function("push_pop_with_arguments", |b| {
        let mut stack = Stack::new().expect("should reserve");
        b.iter(|| {
            stack
                .push(black_box(SMALL_FRAME), black_box(&args), 0)
                .expect("should have room");
            black_box(stack.pop());
        });
    });

    group.throughput(Throughput::Elements(DEPTH as u64));
    group.bench_function("recurse_and_unwind", |b| {
        let mut stack = Stack::new().expect("should reserve");
        stack.push(SMALL_FRAME, &[], 0).expect("should have room");
        b.iter(|| {
            for _ in 0..DEPTH {
                stack
                    .push_call(SMALL_FRAME, THREE)
                    .expect("should have room");
            }
            for _ in 0..DEPTH {
                black_box(stack.pop());
            }
        });
    });

    group.finish();
}

fn registers(c: &mut Criterion) {
    let mut group = c.benchmark_group("register");

    // A hundred frames deep, because a register access is relative to the innermost frame and the
    // benchmark should not accidentally measure the one frame case.
    let mut stack = Stack::new().expect("should reserve");
    for _ in 0..DEPTH {
        stack.push(SMALL_FRAME, &[], 0).expect("should have room");
    }

    group.throughput(Throughput::Elements(1));
    group.bench_function("read", |b| {
        b.iter(|| black_box(stack.get(black_box(Register(3)))));
    });

    group.bench_function("write", |b| {
        b.iter(|| stack.set(black_box(Register(3)), black_box(Value::from_i32(7))));
    });

    // The shape a three address instruction has: two reads and a write. Measured together because
    // that is what the dispatch loop actually costs per instruction, and the parts do not simply
    // add up once the loads are in flight at the same time.
    group.bench_function("read_read_write", |b| {
        b.iter(|| {
            let lhs = stack.get(black_box(Register(1)));
            let rhs = stack.get(black_box(Register(2)));
            stack.set(black_box(Register(3)), black_box(lhs));
            black_box(rhs);
        });
    });

    group.throughput(Throughput::Elements(u64::from(SMALL_FRAME) * DEPTH as u64));
    group.bench_function("root_scan", |b| {
        b.iter(|| {
            let mut pointers = 0_usize;
            for value in black_box(stack.roots()) {
                pointers += usize::from(value.is_pointer());
            }
            pointers
        });
    });

    group.finish();
}

criterion_group!(benches, calls, registers);
criterion_main!(benches);
