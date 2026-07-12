use criterion::{BenchmarkId, Criterion, SamplingMode, criterion_group, criterion_main};
use std::time::Duration;

const TOTAL_EVENTS: u32 = 10000;
const NARROW_TASKS: u32 = 5;
const WIDE_TASKS: u32 = 100;
const WIDE_EVENTS_PER_TASK: u32 = TOTAL_EVENTS / WIDE_TASKS;
const NARROW_EVENTS_PER_TASK: u32 = TOTAL_EVENTS / NARROW_TASKS;
const SEED: u64 = 0x12345678;

const SCALING_TOTAL_EVENTS: [u32; 2] = [1000, 10000];
const SCALING_TASKS: [u32; 4] = [4, 16, 32, 64];

unsafe extern "C" {
    fn run_bench_counter(seed: u64, tasks: u32, events_per_task: u32) -> u32;
}

#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

fn counter(tasks: u32, events_per_task: u32) {
    // SAFETY: `run_bench_counter` is provided by the linked C benchmark fixture
    // and its signature matches this declaration.
    let count = unsafe { run_bench_counter(SEED, tasks, events_per_task) };
    assert_eq!(count, tasks * events_per_task);
}

pub fn counter_sync_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("counter sync");
    g.warm_up_time(Duration::from_millis(100));

    g.bench_function("random-narrow", |b| {
        b.iter(|| counter(NARROW_TASKS, NARROW_EVENTS_PER_TASK))
    });
    g.bench_function("random-wide", |b| {
        b.iter(|| counter(WIDE_TASKS, WIDE_EVENTS_PER_TASK))
    });
}

pub fn counter_scaling_sync_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("counter scaling sync");
    g.sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .sampling_mode(SamplingMode::Flat);

    for num_tasks in SCALING_TASKS {
        for num_total_events in SCALING_TOTAL_EVENTS {
            if num_tasks * 10 >= num_total_events {
                continue;
            }
            let num_events_per_task = num_total_events / num_tasks;
            let parameter_string = format!("{{tasks:{num_tasks},events:{num_total_events}}}");
            g.bench_with_input(
                BenchmarkId::new("RW", parameter_string),
                &(num_tasks, num_events_per_task),
                |b, (num_tasks, num_events_per_task)| {
                    b.iter(|| counter(*num_tasks, *num_events_per_task))
                },
            );
        }
    }
}

criterion_group!(
    benches,
    counter_sync_benchmark,
    counter_scaling_sync_benchmark
);
criterion_main!(benches);
