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
//!
//! The `strings` group measures the same two opcodes with strings in the registers instead of
//! numbers, because `Add` and `Less` are now two opcodes each. `concat_chain` allocates and
//! `string_compare_chain` does not, which is the whole difference between them and the reason they
//! are measured apart. Both of them exist mostly to be read next to `add_chain` and `compare_chain`,
//! since the question a reader has is not what a concatenation costs in isolation but how much the
//! string case costs over the number case that was there before it.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
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
    blueprint_with(code, ConstantPool::default())
}

/// The same, for the bodies that need something in the constant pool to load.
fn blueprint_with(code: Vec<Op>, constants: ConstantPool) -> FunctionBlueprint {
    let out = FunctionBlueprint {
        frame_size: 8,
        cache_slots: 1,
        code,
        constants,
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

/// How many string instructions one measured run performs.
///
/// Shorter than `CHAIN` because a concatenation allocates and nothing collects yet, so the length of
/// this chain is how much heap one measured run consumes. At two hundred it is a few kilobytes,
/// which a fresh isolate has room for many times over, and it is still long enough that the frame
/// push either side of it does not show up in the per instruction number.
const STRING_CHAIN: usize = 200;

/// A chain of one string opcode, with two string constants loaded into registers zero and one.
fn string_chain(build: impl Fn() -> Op) -> FunctionBlueprint {
    let mut constants = ConstantPool::default();
    // Two short Latin-1 strings that differ in their first code unit, so the comparison decides on
    // the first byte it reads and measures the call rather than the length of the string.
    let left = constants.string("katsu ");
    let right = constants.string("runtime");

    let mut code = vec![
        Op::LoadConst {
            dst: Register(0),
            src: left,
        },
        Op::LoadConst {
            dst: Register(1),
            src: right,
        },
    ];
    code.extend((0..STRING_CHAIN).map(|_| build()));
    code.push(Op::Return { src: Register(2) });
    blueprint_with(code, constants)
}

fn strings(c: &mut Criterion) {
    let mut group = c.benchmark_group("strings");
    group.throughput(Throughput::Elements(STRING_CHAIN as u64));

    let concats = string_chain(|| Op::Add {
        dst: Register(2),
        lhs: Register(0),
        rhs: Register(1),
        cache: IC,
    });
    group.bench_function("concat_chain", |b| {
        // A fresh isolate per iteration, because every concatenation allocates and there is no
        // collector to take it back yet. The setup and the drop are outside the timing, so what is
        // measured is still only the run. This is the one benchmark in the workspace whose shape is
        // dictated by a milestone that has not landed, and it goes back to a plain `iter` once M4
        // gives the heap a way to reclaim.
        b.iter_batched(
            || Interpreter::new().expect("should reserve a stack"),
            |mut interpreter| black_box(interpreter.run(black_box(&concats))),
            BatchSize::PerIteration,
        );
    });

    let compares = string_chain(|| Op::Less {
        dst: Register(2),
        lhs: Register(0),
        rhs: Register(1),
        cache: IC,
    });
    group.bench_function("string_compare_chain", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&compares))));
    });

    group.finish();
}

criterion_group!(benches, straight_line, loops, strings);
criterion_main!(benches);
