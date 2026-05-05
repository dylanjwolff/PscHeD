// Integration test for uaf.c
//
// The UAF example has a deliberate data-race window: the reader captures a
// pointer, yields, and then reads magic — while the writer may have changed it
// in the interim.  With enough seeds we expect at least one interleaving to
// expose the race.

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
fn uaf_race_is_detectable() {
    let _g = RSCHED_LOCK.lock().unwrap();
    let detected = (0u64..50).any(|seed| unsafe { run_uaf(seed) } == 1);
    assert!(
        detected,
        "expected at least one seed out of 50 to expose the UAF race, but none did"
    );
}

#[test]
fn uaf_is_deterministic_per_seed() {
    let _g = RSCHED_LOCK.lock().unwrap();
    for seed in 0..5 {
        let a = unsafe { run_uaf(seed) };
        let b = unsafe { run_uaf(seed) };
        assert_eq!(a, b, "seed {seed}: first run={a} but second run={b}");
    }
}
