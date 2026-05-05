// Integration test for uniform.c
//
// Runs run_uniform() with many different seeds and asserts that the scheduler
// explores a diverse set of interleavings (i.e. produces multiple distinct
// final values of x).

use std::collections::HashSet;
use std::os::raw::{c_int, c_ulonglong};
use std::sync::Mutex;

unsafe extern "C" {
    fn run_uniform(seed: c_ulonglong) -> c_int;
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
fn uniform_explores_multiple_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    let mut outcomes = HashSet::new();
    for seed in 0..30 {
        let v = unsafe { run_uniform(seed) };
        outcomes.insert(v);
    }
    assert!(
        outcomes.len() > 3,
        "expected >3 distinct outcomes across 30 seeds, got {}: {:?}",
        outcomes.len(),
        outcomes,
    );
}

/// Determinism check: the same seed always produces the same value.
#[test]
fn uniform_is_deterministic_per_seed() {
    let _g = RSCHED_LOCK.lock().unwrap();
    for seed in 0..5 {
        let a = unsafe { run_uniform(seed) };
        let b = unsafe { run_uniform(seed) };
        assert_eq!(a, b, "seed {seed}: first run={a} but second run={b}");
    }
}
