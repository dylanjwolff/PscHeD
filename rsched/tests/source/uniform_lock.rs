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

fn reset_dfs() {
    // SAFETY: Tests hold `RSCHED_LOCK`, so mutating the process environment
    // and rsched's process-global DFS state does not race with sibling tests.
    unsafe {
        std::env::set_var("RSCHED_SCHEDULER", "dfs");
        rsched::rsched_dfs_reset();
    }
}

fn clear_dfs_env() {
    // SAFETY: Tests hold `RSCHED_LOCK`, so this process environment mutation
    // does not race with sibling tests.
    unsafe {
        std::env::remove_var("RSCHED_SCHEDULER");
    }
}

fn dfs_has_next() -> bool {
    // SAFETY: Serialized by `RSCHED_LOCK`; rsched DFS state is initialized by
    // `reset_dfs` before this helper is called.
    unsafe { rsched::rsched_dfs_has_next() }
}

fn dfs_finish_current() {
    // SAFETY: Serialized by `RSCHED_LOCK`; called exactly once for each
    // completed DFS fixture run.
    unsafe {
        rsched::rsched_dfs_finish_current();
    }
}

fn dfs_completed_schedules() -> usize {
    // SAFETY: Serialized by `RSCHED_LOCK`; reads rsched's process-global DFS
    // completion counter.
    unsafe { rsched::rsched_dfs_completed_schedules() }
}

fn run_uniform_lock_dfs_once() -> c_int {
    // SAFETY: `run_uniform_lock_dfs` is provided by the linked C fixture and
    // its signature matches this declaration.
    unsafe { run_uniform_lock_dfs() }
}

#[test]
fn uniform_lock_dfs_explores_multiple_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    reset_dfs();

    let mut outcomes = HashSet::new();
    let mut runs = 0usize;
    while dfs_has_next() {
        outcomes.insert(run_uniform_lock_dfs_once());
        dfs_finish_current();
        runs += 1;
    }
    clear_dfs_env();

    assert!(
        outcomes.len() > 1,
        "expected >1 distinct outcome under DFS, got {} across {runs} runs: {:?}",
        outcomes.len(),
        outcomes,
    );
    assert_eq!(runs, dfs_completed_schedules());
}
