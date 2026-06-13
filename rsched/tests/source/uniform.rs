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

#[test]
fn uniform_dfs_explores_multiple_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("RSCHED_SCHEDULER", "dfs");
        rsched::rsched_dfs_reset();
    }

    let mut outcomes = HashSet::new();
    let mut runs = 0usize;
    while unsafe { rsched::rsched_dfs_has_next() } {
        outcomes.insert(unsafe { run_uniform_dfs() });
        unsafe {
            rsched::rsched_dfs_finish_current();
        }
        runs += 1;
    }
    unsafe {
        std::env::remove_var("RSCHED_SCHEDULER");
    }

    assert!(
        outcomes.len() > 3,
        "expected >3 distinct outcomes under DFS, got {} across {runs} runs: {:?}",
        outcomes.len(),
        outcomes,
    );
    assert_eq!(runs, unsafe { rsched::rsched_dfs_completed_schedules() });
}

#[test]
fn random_scheduler_is_deterministic_per_seed() {
    let _g = RSCHED_LOCK.lock().unwrap();
    for seed in 0..5 {
        let a = unsafe { run_uniform(seed) };
        let b = unsafe { run_uniform(seed) };
        assert_eq!(a, b, "seed {seed}: first run={a} but second run={b}");
    }
}
