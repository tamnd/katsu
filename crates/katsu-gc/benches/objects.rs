//! Benchmarks for the object model: making an object, reading a property, writing one and growing.
//!
//! Same standing as the other microbenchmarks in this crate. `spec/15-benchmarks.md` says these are
//! regression guards rather than published results, because the published numbers come from real
//! programs run against Node, Deno and Bun in the katsu-bench repository.
//!
//! Four things are worth guarding here. Making an object is the allocation a JavaScript program does
//! more of than any other after strings, and an object literal built with room for its properties has
//! to cost exactly one allocation. Reading a property is the single hottest operation in the language
//! and today it is a walk up the shape's parent chain, so the shape of that curve against the number
//! of properties is the number that says how badly an inline cache is needed. Writing a property that
//! is already there has to not touch the shape at all. Adding a property that some other object has
//! already added has to find the existing transition rather than allocate a new shape, which is the
//! entire reason shapes form a tree, and the difference between that and a first time transition is
//! what says whether the tree is doing its job.
//!
//! The lookup benchmarks are parameterised by property count rather than run at one size, because a
//! parent chain walk is linear and a single number would hide that. When inline caches land the same
//! benchmarks should go flat, and that flattening is the result worth having.
//!
//! Anything that builds a `BumpHeap` per iteration uses `BatchSize::PerIteration`, for the reason
//! written at the top of `heap.rs`: each heap holds its own reservation and a batched setup keeps
//! hundreds of them alive at once.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use katsu_gc::{AtomTable, Attributes, BumpHeap, ObjectRef, ShapeRef, StringRef};

/// How many properties the lookup benchmarks put on an object.
///
/// One is the common case in real code, four is about the median object, and sixteen is where a
/// linear walk should start to look bad next to a hash lookup.
const COUNTS: [u32; 4] = [1, 2, 4, 16];

/// How many elements the element benchmarks put on an object.
///
/// One number rather than the four above, because an element read does not walk anything and its
/// cost is the same at every size. Sixteen so that the append benchmark crosses the doubling
/// threshold twice and the amortisation is actually being measured.
const COUNT: u32 = 16;

/// Objects made per iteration, so the timer has something to measure.
const BATCH: u32 = 1024;

/// How many different names one shape gets asked to transition on, for the fan out benchmark.
const FAN_OUT: u32 = 64;

/// A heap with room already committed, so a benchmark measures the work and not an mmap.
fn heap(bytes: usize) -> BumpHeap {
    let mut heap = BumpHeap::new().unwrap();
    heap.reserve(bytes).unwrap();
    heap
}

/// Interned names, which is what a property key always is by the time it reaches the object model.
fn names(heap: &mut BumpHeap, count: u32) -> Vec<StringRef> {
    let mut atoms = AtomTable::new();
    (0..count)
        .map(|index| {
            atoms
                .intern(heap, &format!("property{index}"))
                .unwrap()
                .as_string()
        })
        .collect()
}

/// An object carrying `names.len()` properties, built with room for all of them.
fn object(heap: &mut BumpHeap, root: ShapeRef, names: &[StringRef]) -> ObjectRef {
    let object = ObjectRef::new(heap, root, u32::try_from(names.len()).unwrap()).unwrap();
    for (index, name) in names.iter().enumerate() {
        object.set(heap, *name, index as u64).unwrap();
    }
    object
}

fn creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("object/new");

    // An empty object with no room in it, which is `{}` and is the floor everything else is measured
    // against. Sixteen bytes of header and nothing else.
    group.bench_function("empty", |b| {
        b.iter_batched(
            || {
                let mut heap = heap(BATCH as usize * 64);
                let root = ShapeRef::root(&mut heap, None).unwrap();
                (heap, root)
            },
            |(mut heap, root)| {
                for _ in 0..BATCH {
                    black_box(ObjectRef::new(&mut heap, root, 0));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // An object literal with four properties in it, built and filled. The number worth watching is
    // this divided by four against the cost of one `set` below, because the difference is the shape
    // transitions, and they should be a lookup in a short child list rather than an allocation after
    // the first object has walked the path.
    for count in COUNTS {
        group.bench_with_input(BenchmarkId::new("filled", count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let mut heap = heap(BATCH as usize * 512);
                    let root = ShapeRef::root(&mut heap, None).unwrap();
                    let names = names(&mut heap, count);
                    // One object first, so the transitions the timed objects need already exist.
                    // Without this the first iteration pays for the whole shape path and every
                    // other one does not, which is a benchmark measuring its own warmup.
                    object(&mut heap, root, &names);
                    (heap, root, names)
                },
                |(mut heap, root, names)| {
                    for _ in 0..BATCH {
                        black_box(object(&mut heap, root, &names));
                    }
                    heap
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

fn lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("object/get");

    // Reading the property that was added last, which is the one at the end of the parent chain and
    // therefore the cheapest walk. This is the best case and it is the one an inline cache has to
    // beat, not the worst case.
    for count in COUNTS {
        group.bench_with_input(BenchmarkId::new("last", count), &count, |b, &count| {
            let mut heap = heap(1 << 20);
            let root = ShapeRef::root(&mut heap, None).unwrap();
            let names = names(&mut heap, count);
            let object = object(&mut heap, root, &names);
            let last = *names.last().unwrap();
            b.iter(|| black_box(object.get(heap.cage(), black_box(last))));
        });
    }

    // Reading the property that was added first, which is the full walk. The gap between this and
    // the line above is what the parent chain costs, and it is the number that should disappear when
    // there is a cache at the access site.
    for count in COUNTS {
        group.bench_with_input(BenchmarkId::new("first", count), &count, |b, &count| {
            let mut heap = heap(1 << 20);
            let root = ShapeRef::root(&mut heap, None).unwrap();
            let names = names(&mut heap, count);
            let object = object(&mut heap, root, &names);
            let first = names[0];
            b.iter(|| black_box(object.get(heap.cage(), black_box(first))));
        });
    }

    // A name that is not on the object, which walks to the root and finds nothing. A miss is not a
    // rare case: it is what every `typeof x.y` and every prototype chain step will do.
    group.bench_function("miss/16", |b| {
        let mut heap = heap(1 << 20);
        let root = ShapeRef::root(&mut heap, None).unwrap();
        let names = names(&mut heap, 17);
        let absent = names[16];
        let object = object(&mut heap, root, &names[..16]);
        b.iter(|| black_box(object.get(heap.cage(), black_box(absent))));
    });

    group.finish();
}

fn store(c: &mut Criterion) {
    let mut group = c.benchmark_group("object/set");

    // Writing a property that is already there, which is a lookup and a store into a slot. No shape
    // work happens on this path at all, and if a change ever makes it happen this number is where it
    // shows up.
    for count in COUNTS {
        group.bench_with_input(BenchmarkId::new("existing", count), &count, |b, &count| {
            let mut heap = heap(1 << 20);
            let root = ShapeRef::root(&mut heap, None).unwrap();
            let names = names(&mut heap, count);
            let object = object(&mut heap, root, &names);
            let first = names[0];
            b.iter(|| black_box(object.set(&mut heap, black_box(first), 7)));
        });
    }

    // Adding a name nobody has added before, which allocates a shape, and adding one that another
    // object already added, which finds it. Both of them are one `set` on a fresh object, so the
    // difference between the two numbers is exactly what the transition tree saves.
    //
    // A first time transition is a failed search of the child list followed by an allocation, and
    // the search gets longer with every name added, so this measures a fan out of sixty four rather
    // than a single transition. Sixty four is a program making objects that share nothing, which is
    // the case the tree is worst at, and dividing this by sixty four understates the last store and
    // overstates the first. The right fix is a child map once the list gets long, and this is the
    // number that would have to move to justify one.
    group.bench_function("add/first_time_fan_out_64", |b| {
        b.iter_batched(
            || {
                let mut heap = heap(1 << 20);
                // A fresh root each iteration, so every one of the sixty four stores below is a
                // transition that has never been taken.
                let root = ShapeRef::root(&mut heap, None).unwrap();
                let names = names(&mut heap, FAN_OUT);
                let objects: Vec<ObjectRef> = (0..FAN_OUT)
                    .map(|_| ObjectRef::new(&mut heap, root, 1).unwrap())
                    .collect();
                (heap, names, objects)
            },
            |(mut heap, names, objects)| {
                for (object, name) in objects.iter().zip(&names) {
                    black_box(object.set(&mut heap, *name, 1));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.bench_function("add/existing_shape", |b| {
        b.iter_batched(
            || {
                let mut heap = heap(BATCH as usize * 256);
                let root = ShapeRef::root(&mut heap, None).unwrap();
                let names = names(&mut heap, 1);
                // Take the transition once, so the timed stores all find it.
                object(&mut heap, root, &names);
                let objects: Vec<ObjectRef> = (0..BATCH)
                    .map(|_| ObjectRef::new(&mut heap, root, 1).unwrap())
                    .collect();
                (heap, names[0], objects)
            },
            |(mut heap, name, objects)| {
                for object in &objects {
                    black_box(object.set(&mut heap, name, 1));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // Growing past the room the object was built with, which is where the overflow array is
    // allocated and then doubled. An object built with no room and given eight properties pays for
    // three arrays, and this is what that costs.
    group.bench_function("grow/0_to_8", |b| {
        b.iter_batched(
            || {
                let mut heap = heap(BATCH as usize * 512);
                let root = ShapeRef::root(&mut heap, None).unwrap();
                let names = names(&mut heap, 8);
                object(&mut heap, root, &names);
                (heap, root, names)
            },
            |(mut heap, root, names)| {
                for _ in 0..BATCH {
                    let object = ObjectRef::new(&mut heap, root, 0).unwrap();
                    for name in &names {
                        black_box(object.set(&mut heap, *name, 1));
                    }
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn transitions(c: &mut Criterion) {
    let mut group = c.benchmark_group("shape");

    // Finding a transition that is already there, which is the operation every object literal after
    // the first one in a program performs. It is a walk down a child list comparing four byte slots,
    // and the list is as long as the number of different names anyone has ever added at this point.
    for width in [1u32, 4, 16] {
        group.bench_with_input(
            BenchmarkId::new("transition/found", width),
            &width,
            |b, &width| {
                let mut heap = heap(1 << 20);
                let root = ShapeRef::root(&mut heap, None).unwrap();
                let names = names(&mut heap, width);
                for name in &names {
                    root.transition(&mut heap, *name, Attributes::DEFAULT)
                        .unwrap();
                }
                // The first name added is the last child in the list, because a new child goes on
                // the front. That makes this the full walk rather than the lucky one.
                let first = names[0];
                b.iter(|| {
                    black_box(root.transition(&mut heap, black_box(first), Attributes::DEFAULT))
                });
            },
        );
    }

    // Asking a shape for the index of a name, which is the parent chain walk on its own with the
    // object out of the picture.
    for count in COUNTS {
        group.bench_with_input(BenchmarkId::new("index_of", count), &count, |b, &count| {
            let mut heap = heap(1 << 20);
            let root = ShapeRef::root(&mut heap, None).unwrap();
            let names = names(&mut heap, count);
            let mut shape = root;
            for name in &names {
                shape = shape
                    .transition(&mut heap, *name, Attributes::DEFAULT)
                    .unwrap();
            }
            let first = names[0];
            b.iter(|| black_box(shape.index_of(heap.cage(), black_box(first))));
        });
    }

    group.finish();
}

fn elements(c: &mut Criterion) {
    let mut group = c.benchmark_group("object/element");

    // Reading an index out of an object that has elements on it. The number to compare against is
    // `object/get/last`, which is the same read through a name, and the gap between them is the
    // entire argument for element storage: this one is a bounds check and a load, and that one is a
    // walk up a parent chain. Neither of them includes the number formatting a computed key pays in
    // the interpreter today, which is the larger half of 7.6.1's 156 nanoseconds.
    group.bench_with_input(BenchmarkId::new("get", COUNT), &COUNT, |b, &count| {
        let mut heap = heap(1 << 20);
        let root = ShapeRef::root(&mut heap, None).unwrap();
        let object = ObjectRef::new(&mut heap, root, 0).unwrap();
        for index in 0..count {
            object.set_element(&mut heap, index, u64::from(index));
        }
        let last = count - 1;
        b.iter(|| black_box(object.element(heap.cage(), black_box(last))));
    });

    // Writing over an index that is already there, which has to touch nothing but the value.
    group.bench_with_input(BenchmarkId::new("set", COUNT), &COUNT, |b, &count| {
        let mut heap = heap(1 << 20);
        let root = ShapeRef::root(&mut heap, None).unwrap();
        let object = ObjectRef::new(&mut heap, root, 0).unwrap();
        for index in 0..count {
            object.set_element(&mut heap, index, u64::from(index));
        }
        let last = count - 1;
        b.iter(|| black_box(object.set_element(&mut heap, black_box(last), 7)));
    });

    // Appending one at a time from nothing, which is the loop that builds an array and the one the
    // doubling exists for. Divided by the count this should be close to the `set` above, because a
    // geometric number of copies amortises to a constant, and the day it stops being close is the
    // day the growth policy stopped doubling.
    group.bench_with_input(BenchmarkId::new("append", COUNT), &COUNT, |b, &count| {
        b.iter_batched(
            || {
                let mut heap = heap(BATCH as usize * 512);
                let root = ShapeRef::root(&mut heap, None).unwrap();
                (heap, root)
            },
            |(mut heap, root)| {
                let object = ObjectRef::new(&mut heap, root, 0).unwrap();
                for index in 0..count {
                    black_box(object.set_element(&mut heap, index, u64::from(index)));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // Asking for the room up front, which is what an array literal knows and a growing loop does
    // not. One allocation against the four the append above pays for the same sixteen values.
    group.bench_with_input(BenchmarkId::new("reserved", COUNT), &COUNT, |b, &count| {
        b.iter_batched(
            || {
                let mut heap = heap(BATCH as usize * 512);
                let root = ShapeRef::root(&mut heap, None).unwrap();
                (heap, root)
            },
            |(mut heap, root)| {
                let object = ObjectRef::new(&mut heap, root, 0).unwrap();
                object.reserve_elements(&mut heap, count).unwrap();
                for index in 0..count {
                    black_box(object.set_element(&mut heap, index, u64::from(index)));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(benches, creation, lookup, store, transitions, elements);
criterion_main!(benches);
