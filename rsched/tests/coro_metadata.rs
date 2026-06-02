#![cfg(feature = "coro")]

use std::sync::Mutex;

unsafe extern "C" {
    fn run_coro_metadata(seed: u64) -> i32;
}

#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

static RSCHED_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn coroutine_threads_preserve_pthread_metadata_and_tls() {
    let _g = RSCHED_LOCK.lock().unwrap();
    for seed in 0..10 {
        let failures = unsafe { run_coro_metadata(seed) };
        assert_eq!(failures, 0, "seed {seed} failed with bitmask {failures:#x}");
    }
}
