use criterion::{Criterion, criterion_group, criterion_main};

const NARROW_TASKS: u32 = 5;
const WIDE_TASKS: u32 = 100;
const SEED: u64 = 0x12345678;

unsafe extern "C" {
    fn run_bench_create(seed: u64, tasks: u32) -> u32;
}

#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

fn create(tasks: u32) {
    let count = unsafe { run_bench_create(SEED, tasks) };
    assert_eq!(count, tasks);
}

pub fn create_sync_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("create sync");

    g.bench_function("random-narrow", |b| b.iter(|| create(NARROW_TASKS)));
    g.bench_function("random-wide", |b| b.iter(|| create(WIDE_TASKS)));
}

criterion_group!(benches, create_sync_benchmark);
criterion_main!(benches);
