// Integration test for the locked uniform interleaving fixture.

use std::collections::HashSet;
use std::os::raw::c_int;
use std::sync::Mutex;

unsafe extern "C" {
    fn run_uniform_lock_dfs() -> c_int;
}

// Force rsched_* symbols into this binary (see tests/uniform.rs for details).
#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

// Serialize tests to avoid races on global rsched state.
static RSCHED_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn uniform_lock_dfs_explores_multiple_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("RSCHED_SCHEDULER", "dfs");
        rsched::rsched_dfs_reset();
    }

    let mut outcomes = HashSet::new();
    let mut runs = 0usize;
    while unsafe { rsched::rsched_dfs_has_next() } {
        outcomes.insert(unsafe { run_uniform_lock_dfs() });
        unsafe {
            rsched::rsched_dfs_finish_current();
        }
        runs += 1;
    }
    unsafe {
        std::env::remove_var("RSCHED_SCHEDULER");
    }

    assert!(
        outcomes.len() > 1,
        "expected >1 distinct outcome under DFS, got {} across {runs} runs: {:?}",
        outcomes.len(),
        outcomes,
    );
    assert_eq!(runs, unsafe { rsched::rsched_dfs_completed_schedules() });
}
