// Integration test for the uniform interleaving fixture.

use std::collections::HashSet;
use std::os::raw::{c_int, c_ulonglong};
use std::sync::Mutex;

unsafe extern "C" {
    fn run_uniform(seed: c_ulonglong) -> c_int;
    fn run_uniform_dfs() -> c_int;
}

// Force the rsched_* CGU into this binary so that libcexamples.a can resolve
// its (undefined) rsched_* symbols at link time.  Without this reference,
// rustc sees no Rust-side use of the rsched crate and omits its object code.
#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

// rsched has global state; serialize all tests within this binary so they
// don't race on STATE / GMTX / INITED when the test framework threads them.
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

fn run_uniform_dfs_once() -> c_int {
    // SAFETY: `run_uniform_dfs` is provided by the linked C fixture and its
    // signature matches this declaration.
    unsafe { run_uniform_dfs() }
}

fn run_uniform_seed(seed: c_ulonglong) -> c_int {
    // SAFETY: `run_uniform` is provided by the linked C fixture and its
    // signature matches this declaration.
    unsafe { run_uniform(seed) }
}

#[test]
fn uniform_dfs_explores_multiple_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    reset_dfs();

    let mut outcomes = HashSet::new();
    let mut runs = 0usize;
    while dfs_has_next() {
        outcomes.insert(run_uniform_dfs_once());
        dfs_finish_current();
        runs += 1;
    }
    clear_dfs_env();

    assert!(
        outcomes.len() > 3,
        "expected >3 distinct outcomes under DFS, got {} across {runs} runs: {:?}",
        outcomes.len(),
        outcomes,
    );
    assert_eq!(runs, dfs_completed_schedules());
}

#[test]
fn random_scheduler_is_deterministic_per_seed() {
    let _g = RSCHED_LOCK.lock().unwrap();
    for seed in 0..5 {
        let a = run_uniform_seed(seed);
        let b = run_uniform_seed(seed);
        assert_eq!(a, b, "seed {seed}: first run={a} but second run={b}");
    }
}
