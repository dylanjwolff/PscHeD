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
    unsafe {
        std::env::set_var("RSCHED_SCHEDULER", "dfs");
        rsched::rsched_dfs_reset();
    }

    let mut runs = 0usize;
    while unsafe { rsched::rsched_dfs_has_next() } {
        let failures = unsafe { run_coro_metadata(0) };
        assert_eq!(
            failures, 0,
            "DFS run {runs} failed with bitmask {failures:#x}"
        );
        unsafe {
            rsched::rsched_dfs_finish_current();
        }
        runs += 1;
    }

    unsafe {
        std::env::remove_var("RSCHED_SCHEDULER");
    }
    assert!(runs > 0, "DFS did not run the coroutine metadata fixture");
    assert_eq!(runs, unsafe { rsched::rsched_dfs_completed_schedules() });
}
