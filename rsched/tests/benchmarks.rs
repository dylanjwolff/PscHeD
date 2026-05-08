use std::sync::Mutex;

unsafe extern "C" {
    fn run_bench_create(seed: u64, tasks: u32) -> u32;
    fn run_bench_counter(seed: u64, tasks: u32, events_per_task: u32) -> u32;
    fn run_bench_lock(seed: u64, tasks: u32, events_per_task: u32) -> u32;
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

static RSCHED_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn c_benchmark_kernels_complete() {
    let _g = RSCHED_LOCK.lock().unwrap();
    unsafe {
        assert_eq!(run_bench_create(0x12345678, 8), 8);
        assert_eq!(run_bench_counter(0x12345678, 8, 4), 32);
        assert_eq!(run_bench_lock(0x12345678, 8, 4), 32);
        assert_eq!(run_bench_buffer(0x12345678, 2, 2, 8, 3), 0);
    }
}
