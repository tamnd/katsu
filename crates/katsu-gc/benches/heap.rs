//! Benchmarks for bump allocation and for compressing values into slots.
//!
//! Same standing as the value benchmarks: these are microbenchmarks, `spec/15-benchmarks.md`
//! says microbenchmarks are not published as results, and they are here as a regression guard.
//!
//! There are two numbers worth guarding. Allocation cost per object is one, because a JavaScript
//! program allocates constantly and this heap is the floor every later collector gets measured
//! against. Note that the allocation figure includes the census bookkeeping, because the census
//! is on the allocation path rather than sampled and there is no path that skips it. That was a
//! deliberate choice, on the grounds that a sampled allocation count is not something a memory
//! budget test can fail on, and the cost of it is inside this number rather than hidden next to
//! it.
//!
//! The other is compression and decompression, which run on every property load and store and
//! are meant to be a bitwise or and a subtraction. The fast and slow allocation paths are
//! measured separately, because an average of the two is a number that describes neither.
//!
//! Every group that needs a fresh heap uses `BatchSize::PerIteration`. `SmallInput` would run the
//! setup for a whole batch before timing any of it, and since each heap owns an eight gigabyte
//! reservation that means hundreds of them alive at once. macOS accepts that and Linux refuses it
//! with ENOMEM, which is what the first run on the x86 reference machine reported.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use katsu_gc::{BumpHeap, Cage, ObjectKind, Slot};

/// Allocations per iteration. Enough to be above the timer, small enough that the four gigabyte
/// cage lasts for a full criterion run at these object sizes.
const BATCH: u32 = 4096;

fn allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("allocate");

    // Twenty four bytes is the empty object line from the 2.3 memory budget, so this is the
    // allocation the runtime will do more of than any other. The pages are committed in the setup
    // so that this measures the bump and the census bookkeeping rather than an mmap amortised
    // over a few thousand allocations.
    group.bench_function("empty_object_24b", |b| {
        b.iter_batched(
            || {
                let mut heap = BumpHeap::new().unwrap();
                heap.reserve(BATCH as usize * 32).unwrap();
                heap
            },
            |mut heap| {
                for _ in 0..BATCH {
                    black_box(heap.allocate(24, ObjectKind::Object));
                }
                heap
            },
            // PerIteration, not SmallInput, and the same everywhere below. See the note at the
            // top of the file about why a batched setup cannot be used here.
            criterion::BatchSize::PerIteration,
        );
    });

    // A size that is not a multiple of the alignment, so the rounding is on the path.
    group.bench_function("unaligned_37b", |b| {
        b.iter_batched(
            || {
                let mut heap = BumpHeap::new().unwrap();
                heap.reserve(BATCH as usize * 48).unwrap();
                heap
            },
            |mut heap| {
                for _ in 0..BATCH {
                    black_box(heap.allocate(37, ObjectKind::String));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // Large enough that every allocation crosses a commit chunk, which puts an mmap on every
    // iteration. This is the slow path and it is measured so that the fast path number above can
    // be read as a fast path number rather than as an average of the two.
    group.bench_function("commit_bound_96kb", |b| {
        b.iter_batched(
            || BumpHeap::new().unwrap(),
            |mut heap| {
                for _ in 0..64 {
                    black_box(heap.allocate(96 * 1024, ObjectKind::Elements));
                }
                heap
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("slot");

    let cage = Cage::new().unwrap();

    // Decompression is the hot direction. It runs on every property load, and it is meant to be
    // one bitwise or because the cage base has a clear low half.
    let offsets: Vec<u32> = (0..BATCH).map(|i| i.wrapping_mul(8)).collect();
    group.bench_function("decompress", |b| {
        b.iter(|| {
            // The addresses are summed rather than overwritten. Keeping only the last one lets
            // the optimiser drop every other iteration, which it did, and the benchmark then
            // reported a picosecond for four thousand decompressions.
            let mut total = 0usize;
            for &offset in black_box(&offsets) {
                total = total.wrapping_add(cage.address_of(offset) as usize);
            }
            black_box(total)
        });
    });

    let addresses: Vec<*mut u8> = offsets.iter().map(|&o| cage.address_of(o)).collect();
    group.bench_function("compress", |b| {
        b.iter(|| {
            let mut total = 0u64;
            for &address in black_box(&addresses) {
                total += u64::from(cage.offset_of(address).unwrap_or(0));
            }
            black_box(total)
        });
    });

    // The tagging itself, separately from the address arithmetic, so a regression in one is not
    // hidden by the other.
    let integers: Vec<i32> = (0..BATCH)
        .map(|i| i.cast_signed().wrapping_mul(31) - 60_000)
        .collect();
    group.bench_function("smi_round_trip", |b| {
        b.iter(|| {
            let mut total = 0i64;
            for &n in black_box(&integers) {
                let slot = Slot::from_smi(n).unwrap_or(Slot::ZERO);
                total = total.wrapping_add(i64::from(slot.as_smi().unwrap_or(0)));
            }
            black_box(total)
        });
    });

    group.finish();
}

criterion_group!(benches, allocation, compression);
criterion_main!(benches);
