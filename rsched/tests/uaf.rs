// Integration test for the UAF race fixture.

use std::os::raw::{c_int, c_ulonglong};
use std::sync::Mutex;

unsafe extern "C" {
    fn run_uaf(seed: c_ulonglong) -> c_int;
}

// Force rsched_* symbols into this binary (see tests/uniform.rs for details).
#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

// Serialize tests to avoid races on global rsched state.
static RSCHED_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn uaf_race_is_detectable_under_dfs() {
    let _g = RSCHED_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("RSCHED_SCHEDULER", "dfs");
        rsched::rsched_dfs_reset();
    }

    let mut detected = false;
    let mut runs = 0usize;
    while unsafe { rsched::rsched_dfs_has_next() } {
        detected |= unsafe { run_uaf(0) } == 1;
        unsafe {
            rsched::rsched_dfs_finish_current();
        }
        runs += 1;
    }
    unsafe {
        std::env::remove_var("RSCHED_SCHEDULER");
    }

    assert!(
        detected,
        "expected DFS to expose the UAF race, but none of {runs} schedules did"
    );
    assert_eq!(runs, unsafe { rsched::rsched_dfs_completed_schedules() });
}
