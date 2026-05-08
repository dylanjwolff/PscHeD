use criterion::{criterion_group, criterion_main, Criterion};

const SEED: u64 = 0x12345678;
const NUM_PRODUCERS: u32 = 3;
const NUM_CONSUMERS: u32 = 3;
const NUM_EVENTS: u32 = NUM_PRODUCERS * NUM_CONSUMERS * 3;
const MAX_QUEUE_SIZE: u32 = 3;

unsafe extern "C" {
    fn run_bench_buffer(
        seed: u64,
        producers: u32,
        consumers: u32,
        total_events: u32,
        queue_size: u32,
    ) -> u32;
}

#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

fn bounded_buffer() {
    let count = unsafe {
        run_bench_buffer(
            SEED,
            NUM_PRODUCERS,
            NUM_CONSUMERS,
            NUM_EVENTS,
            MAX_QUEUE_SIZE,
        )
    };
    assert_eq!(count, 0);
}

pub fn bounded_buffer_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("buffer");
    g.bench_function("random", |b| b.iter(bounded_buffer));
}

criterion_group!(benches, bounded_buffer_benchmark);
criterion_main!(benches);
