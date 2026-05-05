// Integration test for uniform-lock.c
//
// Like uniform.c but with a mutex guarding part of each thread's critical
// section.  We still expect multiple distinct outcomes (the mutex only
// constrains a subset of the operations), and we verify determinism.

use std::collections::HashSet;
use std::os::raw::{c_int, c_ulonglong};
use std::sync::Mutex;

unsafe extern "C" {
    fn run_uniform_lock(seed: c_ulonglong) -> c_int;
}

// Force rsched_* symbols into this binary (see tests/uniform.rs for details).
#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

// Serialize tests to avoid races on global rsched state.
static RSCHED_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn uniform_lock_explores_multiple_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    let mut outcomes = HashSet::new();
    for seed in 0..30 {
        let v = unsafe { run_uniform_lock(seed) };
        outcomes.insert(v);
    }
    assert!(
        outcomes.len() > 3,
        "expected >3 distinct outcomes across 30 seeds, got {}: {:?}",
        outcomes.len(),
        outcomes,
    );
}

#[test]
fn uniform_lock_is_deterministic_per_seed() {
    let _g = RSCHED_LOCK.lock().unwrap();
    for seed in 0..5 {
        let a = unsafe { run_uniform_lock(seed) };
        let b = unsafe { run_uniform_lock(seed) };
        assert_eq!(a, b, "seed {seed}: first run={a} but second run={b}");
    }
}
