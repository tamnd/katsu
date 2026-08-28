//! Benchmarks for the dispatch loop.
//!
//! Same standing as the other microbenchmarks in this workspace: `spec/15-benchmarks.md` says these
//! are regression guards and not published results.
//!
//! The number that matters here is nanoseconds per instruction, which is why every group reports a
//! throughput in instructions rather than in iterations. It is the number spec 5.3 promised to
//! measure strategy A at, and it is the baseline the threaded and tail called strategies have to
//! beat before either of them is worth the portability they cost.
//!
//! Four things are measured and each one isolates a different cost.
//!
//! `move_chain` is a straight run of the cheapest instruction there is. A move reads one register
//! and writes another and does nothing else, so what is left is the fetch, the bounds check and the
//! branch the match compiles into. That is the floor, and no opcode can be faster than it.
//!
//! `add_chain` is the same shape with real work in it. The gap between it and `move_chain` is what
//! an arithmetic opcode costs on top of dispatch, which is the number that says whether the
//! conversion through `ToNumber` is being folded away or is being paid every time.
//!
//! `counting_loop` is the shape a real program spends its time in: a comparison, a conditional
//! jump, an add and a back edge, four instructions with a dependency between every pair of them.
//! It is the only one of the four where the branch predictor has something to get right and the
//! only one that pays the interrupt check.
//!
//! `pow_chain` is there because exponentiation is the one arithmetic opcode that calls a libm
//! function rather than compiling to an instruction, and it is worth knowing how far outside the
//! others it sits.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use katsu_ir::{CacheIndex, CodeOffset, ConstantPool, FunctionBlueprint, Op, Register};
use katsu_vm::Interpreter;

/// How many instructions a straight line body holds, long enough that the frame push either side of
/// it is noise and short enough to stay well inside the instruction cache.
const CHAIN: usize = 1_000;

/// How many times the counting loop goes round.
const ITERATIONS: i32 = 1_000;

/// Instructions in the body of the counting loop: the compare, the branch, the add and the edge.
const LOOP_BODY: u64 = 4;

const IC: CacheIndex = CacheIndex(0);

/// Wrap a body in a blueprint and check it before handing it to the interpreter, so a mistake in a
/// benchmark fails as a bad blueprint rather than as a surprising number.
fn blueprint(code: Vec<Op>) -> FunctionBlueprint {
    let out = FunctionBlueprint {
        frame_size: 8,
        cache_slots: 1,
        code,
        constants: ConstantPool::default(),
        ..FunctionBlueprint::default()
    };
    out.verify().expect("the benchmark assembled bad bytecode");
    out
}

/// A straight run of one instruction, built by a closure, ending in a return.
fn chain(build: impl Fn() -> Op) -> FunctionBlueprint {
    let mut code = vec![
        Op::LoadInt {
            dst: Register(0),
            value: 1,
        },
        Op::LoadInt {
            dst: Register(1),
            value: 2,
        },
    ];
    code.extend((0..CHAIN).map(|_| build()));
    code.push(Op::Return { src: Register(2) });
    blueprint(code)
}

fn straight_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("dispatch");
    group.throughput(Throughput::Elements(CHAIN as u64));

    let moves = chain(|| Op::Move {
        dst: Register(2),
        src: Register(0),
    });
    group.bench_function("move_chain", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&moves))));
    });

    let adds = chain(|| Op::Add {
        dst: Register(2),
        lhs: Register(0),
        rhs: Register(1),
        cache: IC,
    });
    group.bench_function("add_chain", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&adds))));
    });

    let compares = chain(|| Op::Less {
        dst: Register(2),
        lhs: Register(0),
        rhs: Register(1),
        cache: IC,
    });
    group.bench_function("compare_chain", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&compares))));
    });

    let powers = chain(|| Op::Pow {
        dst: Register(2),
        lhs: Register(0),
        rhs: Register(1),
        cache: IC,
    });
    group.bench_function("pow_chain", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&powers))));
    });

    group.finish();
}

fn loops(c: &mut Criterion) {
    let mut group = c.benchmark_group("loop");

    // `let i = 0; while (i < n) i = i + 1;` with the constant one hoisted, because lowering hoists
    // it too and a load inside the body would be measuring the load.
    let counting = blueprint(vec![
        Op::LoadInt {
            dst: Register(0),
            value: 0,
        },
        Op::LoadInt {
            dst: Register(1),
            value: ITERATIONS,
        },
        Op::LoadInt {
            dst: Register(3),
            value: 1,
        },
        Op::Less {
            dst: Register(2),
            lhs: Register(0),
            rhs: Register(1),
            cache: IC,
        },
        Op::JumpIfFalse {
            cond: Register(2),
            target: CodeOffset(7),
        },
        Op::Add {
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(3),
            cache: IC,
        },
        Op::LoopBackEdge {
            target: CodeOffset(3),
            profile: IC,
        },
        Op::Return { src: Register(0) },
    ]);

    group.throughput(Throughput::Elements(ITERATIONS as u64 * LOOP_BODY));
    group.bench_function("counting_loop", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&counting))));
    });

    // The same loop counted per iteration rather than per instruction, because an iteration is what
    // a JavaScript programmer thinks in and it is the number tier 1 will be compared on.
    group.throughput(Throughput::Elements(ITERATIONS as u64));
    group.bench_function("counting_loop_per_iteration", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&counting))));
    });

    group.finish();
}

criterion_group!(benches, straight_line, loops);
criterion_main!(benches);
