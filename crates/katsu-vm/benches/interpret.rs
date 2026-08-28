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
//!
//! The `call` group reports nanoseconds per call rather than per instruction, because a call is not
//! one instruction: it is a frame pushed, arguments copied, three locals in the dispatch loop moved
//! and all of it undone on the way back. It is the cost every program pays constantly and it is
//! where an interpreter is usually slow, so it is measured on its own. `call_return` is the floor,
//! `fib` is the same thing with real recursion and a captured self reference around it, and
//! `closure_call` adds a read through the environment so that the cost of capturing is visible next
//! to the cost of calling.
//!
//! The `globals` group is the other half of a name lookup. A local is an index into the frame and
//! costs nothing worth measuring, and a global is a hash table, so this is the first name in the
//! language that is not free. It is measured per instruction like the dispatch group, and the number
//! to read it against is `move_chain`, which is the same instruction shape with no lookup in it.
//!
//! The `property` group is the operation spec 4 says the whole architecture is judged on, measured
//! in the form it has before there are shapes to cache against. A record's lookup is a linear scan
//! over interned addresses, so `prop_load` is what eight four byte compares in one cache line cost,
//! and it is the number M1's inline caches have to beat rather than a placeholder to be replaced
//! without checking. `method_call` is the same lookup with a call on the end of it, which is the
//! most common call shape in real code and is why it is one opcode rather than two.
//!
//! The `object` group is what building an object costs, reported per object rather than per
//! instruction, since a literal is a `new_object` and one store per property rather than one
//! opcode. `literal_empty` is the allocation on its own and the other two say what a property adds
//! to it, and `grown_4` builds the same four property object out of an empty literal and four
//! separate stores so that what the room in `new_object` is worth is a difference between two
//! measured numbers rather than an argument. `no_object` is a body of the same length with no
//! object in it at all, and it is there to be subtracted: the isolate these run in is fresh every
//! iteration and standing one up is not free, so the baseline makes that cost visible instead of
//! letting it sit inside the four numbers underneath it.
//!
//! The `native` group is a call that leaves the interpreter. It has the same shape as `call_return`
//! on purpose: the callee is already in a register and the body does nothing, so the difference
//! between the two is the difference between pushing a frame and making a Rust call. Every builtin
//! in the runtime will pay this, so it is worth knowing before there are any.
//!
//! The `switch` group measures what a switch costs today, which is a run of `StrictEqual` and
//! `JumpIfTrue` walked until one of them is true. That is a linear scan, so the two numbers to read
//! together are the first clause and the last: the gap between them is what each clause not taken
//! costs, and it is the number a jump table would have to beat to be worth building. Reported per
//! clause tested rather than per switch, so the two shapes are directly comparable.
//!
//! The call benchmarks come from source rather than from assembled bytecode, because a call is the
//! one place where the cost depends on what lowering and scope analysis decided, and hand written
//! bytecode would measure my guess at those decisions.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use katsu_ir::{CacheIndex, CodeOffset, ConstantPool, FunctionBlueprint, Op, Register};
use katsu_vm::{Interpreter, RuntimeError, Value};

/// How many instructions a straight line body holds, long enough that the frame push either side of
/// it is noise and short enough to stay well inside the instruction cache.
const CHAIN: usize = 1_000;

/// How many times the counting loop goes round.
const ITERATIONS: i32 = 1_000;

/// Instructions in the body of the counting loop: the compare, the branch, the add and the edge.
const LOOP_BODY: u64 = 4;

/// How many clauses in the benchmarked switch.
///
/// Eight, because a real switch is small. A hundred clause switch exists and it is a dispatch table
/// somebody wrote by hand, and measuring one would say more about the cache than about the opcodes.
const CLAUSES: i32 = 8;

/// How many times the switch benchmark goes round its loop, so that one measurement is a run of
/// switches rather than a frame push either side of a handful of instructions.
const SWITCHES: usize = 1_000;

/// How many calls in a straight chain of them, for the same reason `CHAIN` is what it is.
const CALLS: usize = 1_000;

/// How many calls a `fib(20)` makes.
///
/// The count satisfies `C(n) = 1 + C(n - 1) + C(n - 2)` with `C(0)` and `C(1)` both one, which
/// closes to `2 * F(n + 1) - 1`, and `F(21)` is 10946. Spelled out because the division that turns
/// a wall clock number into nanoseconds per call is only as trustworthy as this constant.
const FIB_CALLS: u64 = 21_891;

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

/// Compile a program the way the runtime does, and check it before it is timed.
fn program(source: &str) -> FunctionBlueprint {
    let out = katsu_vm::compile("bench.js", source).expect("the benchmark wrote bad JavaScript");
    out.verify().expect("lowering produced bad bytecode");
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

fn calls(c: &mut Criterion) {
    let mut group = c.benchmark_group("call");

    // A chain of calls rather than a loop around one, so that nothing but the call is being timed.
    // The callee returns its argument, which is the shortest body a real function can have.
    let mut source = String::from("function id(x) { return x; }\n");
    for _ in 0..CALLS {
        source.push_str("id(1);\n");
    }
    let chain = program(&source);
    group.throughput(Throughput::Elements(CALLS as u64));
    group.bench_function("call_return", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&chain))));
    });

    // The same call with a read through the environment inside it, which is what any function that
    // uses a variable from where it was written pays on top.
    let mut source = String::from(
        "function make() { let base = 1; function add(x) { return x + base; } return add; }\nconst add = make();\n",
    );
    for _ in 0..CALLS {
        source.push_str("add(1);\n");
    }
    let captured = program(&source);
    group.bench_function("closure_call", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&captured))));
    });

    // Recursion, which is calls with a real stack under them and a self reference read out of the
    // environment at every one. The oldest benchmark in the business and still the one that says
    // whether a call is cheap.
    let fib = program(
        "function fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); }\nfib(20);",
    );
    group.throughput(Throughput::Elements(FIB_CALLS));
    group.bench_function("fib", |b| {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        b.iter(|| black_box(interpreter.run(black_box(&fib))));
    });

    group.finish();
}

/// A native that does nothing at all, so that a call to it measures the boundary and not the body.
///
/// The `Result` is the signature every native has and not a return this one needs, which is exactly
/// what makes it the right shape to measure the boundary with.
#[allow(clippy::unnecessary_wraps)]
fn nothing(_: &mut Interpreter, _: &[Value]) -> Result<Value, RuntimeError> {
    Ok(Value::UNDEFINED)
}

/// How many names the benchmarked object holds.
///
/// Eight, because that is about what a real host object has on it and because the lookup is a linear
/// scan over interned addresses. Eight four byte names is thirty two bytes, so the whole scan is
/// inside one cache line and the number below is what that is worth.
const PROPERTIES: usize = 8;

/// An interpreter with a global to read, a native to call and an object to read properties off.
///
/// The object's names are `p0` through `p7` and every benchmark below uses `p7`, the last one, which
/// is the worst case for a scan and therefore the honest number to publish.
fn realm() -> Interpreter {
    let mut interpreter = Interpreter::new().expect("should reserve a stack");
    interpreter
        .define_global("answer", Value::from_i32(42))
        .expect("should have room");
    interpreter
        .define_native("nothing", nothing)
        .expect("should have room");

    let call = interpreter
        .native_function("nothing", nothing)
        .expect("should have room");
    let names: Vec<String> = (0..PROPERTIES).map(|index| format!("p{index}")).collect();
    let mut entries: Vec<(&str, Value)> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            (
                name.as_str(),
                i32::try_from(index).map_or(Value::UNDEFINED, Value::from_i32),
            )
        })
        .collect();
    // The last name holds the function, so `host.p7` and `host.p7()` walk the same distance and the
    // difference between the two numbers is the call and nothing else.
    entries[PROPERTIES - 1].1 = call;
    let host = interpreter.host_object(&entries).expect("should have room");
    interpreter
        .define_global("host", host)
        .expect("should have room");
    interpreter
}

fn globals(c: &mut Criterion) {
    let mut group = c.benchmark_group("globals");
    group.throughput(Throughput::Elements(CHAIN as u64));

    // A name that is bound, read over and over. The hash is of four bytes and not of the text,
    // because the name in the constant pool was interned when the unit loaded, and the number here
    // is what says whether that was worth doing.
    let mut source = String::new();
    for _ in 0..CHAIN {
        source.push_str("answer;\n");
    }
    let loads = program(&source);
    group.bench_function("global_load", |b| {
        let mut interpreter = realm();
        b.iter(|| black_box(interpreter.run(black_box(&loads))));
    });

    // Two instructions per line rather than one, the constant and the store, so this sits next to
    // `move_chain` rather than next to `global_load`. What it says is that writing a global is a
    // hash and an insert into a table that already has the key.
    let mut source = String::new();
    for _ in 0..CHAIN {
        source.push_str("answer = 1;\n");
    }
    let stores = program(&source);
    group.bench_function("global_store", |b| {
        let mut interpreter = realm();
        b.iter(|| black_box(interpreter.run(black_box(&stores))));
    });

    group.finish();
}

fn properties(c: &mut Criterion) {
    let mut group = c.benchmark_group("property");
    group.throughput(Throughput::Elements(CHAIN as u64));

    // The operation the whole architecture is judged on, in the form it has before there are shapes.
    // The number to read it against is `global_load`, because both of them are a name lookup and the
    // question M1 has to answer is whether a shape and an inline cache beat a scan of eight
    // addresses in one cache line.
    let mut source = String::new();
    for _ in 0..CHAIN {
        source.push_str("host.p7;\n");
    }
    let loads = program(&source);
    group.bench_function("prop_load", |b| {
        let mut interpreter = realm();
        b.iter(|| black_box(interpreter.run(black_box(&loads))));
    });

    // Two instructions per line, the constant and the store, which is the same shape `global_store`
    // has and is why the two sit next to each other. The same last name as the read, so the two of
    // them walk the same distance and the difference between them is the write.
    let mut source = String::new();
    for _ in 0..CHAIN {
        source.push_str("host.p7 = 1;\n");
    }
    let stores = program(&source);
    group.bench_function("prop_store", |b| {
        let mut interpreter = realm();
        b.iter(|| black_box(interpreter.run(black_box(&stores))));
    });

    // A property read and a call in one opcode, which is the most common call shape in real code.
    // Against `native_call`, the difference is the lookup, and against `prop_load` it is the call.
    group.throughput(Throughput::Elements(CALLS as u64));
    let mut source = String::new();
    for _ in 0..CALLS {
        source.push_str("host.p7(1);\n");
    }
    let calls = program(&source);
    group.bench_function("method_call", |b| {
        let mut interpreter = realm();
        b.iter(|| black_box(interpreter.run(black_box(&calls))));
    });

    group.finish();
}

/// How many objects one body in the `object` group builds.
///
/// A thousand, chosen by measuring rather than by taste. Every one of these allocates and there is
/// no collector to take the memory back yet, so each iteration runs in a fresh isolate, and the cost
/// of standing an isolate up is fixed however long the body is. At two hundred objects that fixed
/// cost was most of the measurement and every number in the group came out several times too large.
/// A thousand pushes it down far enough that the `no_object` baseline is a small correction rather
/// than the bulk of the answer, and a four property object still only reaches forty eight bytes, so
/// the whole group fits in well under a megabyte per iteration.
const LITERALS: usize = 1_000;

fn objects(c: &mut Criterion) {
    let mut group = c.benchmark_group("object");
    group.throughput(Throughput::Elements(LITERALS as u64));

    // Reported per object built rather than per instruction, because an object literal is not one
    // instruction and the question a reader has is what one object costs. `literal_empty` is the
    // allocation and the transition to the root shape and nothing else, so the gap between it and
    // the other two is what a property costs at the interpreter level, which is a store, a
    // transition lookup and a write.
    //
    // `grown_4` is the pair that matters. It builds the same four property object out of an empty
    // literal and four separate stores, so it walks the same transition path and lands on the same
    // shape node, and the only thing it does differently is start with no room, which means its
    // last properties end up in an overflow array. Everything the object model bought is in the
    // difference between those two numbers, and it is the number that says whether the count in
    // `new_object` is earning its place in the instruction.
    //
    // `no_object` is here because every body in this group is timed with a fresh isolate around it,
    // which puts the first touch of newly reserved pages inside the measurement. That cost is the
    // same in all five and it is not what anybody wants to read, so it is measured on its own and
    // subtracted rather than left silently inside the other four.
    for (name, body) in [
        ("no_object", "x = 1;\n"),
        ("literal_empty", "x = {};\n"),
        ("literal_2", "x = { a: 1, b: 2 };\n"),
        ("literal_4", "x = { a: 1, b: 2, c: 3, d: 4 };\n"),
        ("grown_4", "x = {}; x.a = 1; x.b = 2; x.c = 3; x.d = 4;\n"),
    ] {
        let mut source = String::from("let x;\n");
        for _ in 0..LITERALS {
            source.push_str(body);
        }
        let built = program(&source);
        group.bench_function(name, |b| {
            b.iter_batched(
                || Interpreter::new().expect("should reserve a stack"),
                |mut interpreter| black_box(interpreter.run(black_box(&built))),
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

fn natives(c: &mut Criterion) {
    let mut group = c.benchmark_group("native");
    group.throughput(Throughput::Elements(CALLS as u64));

    // The same shape as `call_return` so the two numbers can be read against each other: the callee
    // is in a register, one argument goes with it, and the body does nothing. What is left is the
    // difference between pushing a frame and leaving the loop for a Rust call.
    let mut source = String::from("const f = nothing;\n");
    for _ in 0..CALLS {
        source.push_str("f(1);\n");
    }
    let chain = program(&source);
    group.bench_function("native_call", |b| {
        let mut interpreter = realm();
        b.iter(|| black_box(interpreter.run(black_box(&chain))));
    });

    group.finish();
}

/// A run of identical eight clause switches over a constant, which matches clause `hit`.
///
/// The two shapes this is called with emit exactly the same instructions and differ only in the
/// value being switched on, so the difference between their timings is the cost of the clauses
/// walked past and nothing else. The subject is loaded once outside the chain, because reloading it
/// per switch would put a `LoadInt` into a measurement that is about the comparisons.
fn switch_chain(hit: i32) -> FunctionBlueprint {
    // r0 holds the subject, r1 the clause value being tested against it, r2 the comparison result.
    let mut code = vec![Op::LoadInt {
        dst: Register(0),
        value: hit,
    }];

    // Every clause body is empty, so a switch is its comparisons and the jump past them, and what
    // is measured is the scan rather than anything the clauses do.
    let clauses = usize::try_from(CLAUSES).expect("a clause count is small");
    let per_switch = clauses * 3 + 1;
    for index in 0..SWITCHES {
        let after = u32::try_from(1 + (index + 1) * per_switch).expect("a benchmark body is short");
        for clause in 0..CLAUSES {
            code.push(Op::LoadInt {
                dst: Register(1),
                value: clause,
            });
            code.push(Op::StrictEqual {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                cache: IC,
            });
            code.push(Op::JumpIfTrue {
                cond: Register(2),
                target: CodeOffset(after),
            });
        }
        code.push(Op::Jump {
            target: CodeOffset(after),
        });
    }

    code.push(Op::Return { src: Register(0) });
    blueprint(code)
}

fn switches(c: &mut Criterion) {
    let mut group = c.benchmark_group("switch");

    for (name, hit) in [
        ("switch_first_clause", 0),
        ("switch_last_clause", CLAUSES - 1),
    ] {
        let body = switch_chain(hit);
        // Per clause tested rather than per switch, so that the two shapes are on the same scale
        // and the number to compare them by is what one clause not taken costs.
        let tested = u64::try_from(hit + 1).expect("a clause count is small");
        group.throughput(Throughput::Elements(SWITCHES as u64 * tested));
        group.bench_function(name, |b| {
            let mut interpreter = Interpreter::new().expect("should reserve a stack");
            b.iter(|| black_box(interpreter.run(black_box(&body))));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    straight_line,
    loops,
    switches,
    strings,
    calls,
    globals,
    properties,
    objects,
    natives
);
criterion_main!(benches);
