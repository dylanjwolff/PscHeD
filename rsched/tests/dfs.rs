use std::collections::HashSet;
use std::os::raw::c_int;
use std::sync::Mutex;

unsafe extern "C" {
    fn run_dfs_count() -> c_int;
}

#[used]
static _RSCHED_ANCHOR: unsafe extern "C" fn() = rsched::rsched_init;

static RSCHED_LOCK: Mutex<()> = Mutex::new(());
#[cfg(feature = "coro")]
const EXPECTED_SCHEDULES: usize = 116;
#[cfg(not(feature = "coro"))]
const EXPECTED_SCHEDULES: usize = 122;

#[test]
fn dfs_exhausts_two_by_two_c_interleavings() {
    let _g = RSCHED_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("RSCHED_SCHEDULER", "dfs");
        rsched::rsched_dfs_reset();
    }

    let mut runs = 0usize;
    let mut outcomes = HashSet::new();
    while unsafe { rsched::rsched_dfs_has_next() } {
        outcomes.insert(unsafe { run_dfs_count() });
        unsafe {
            rsched::rsched_dfs_finish_current();
        }
        runs += 1;
    }

    unsafe {
        std::env::remove_var("RSCHED_SCHEDULER");
    }
    assert_eq!(
        outcomes,
        HashSet::from([1122, 1212, 1221, 2112, 2121, 2211])
    );
    assert_eq!(runs, EXPECTED_SCHEDULES);
    assert_eq!(runs, unsafe { rsched::rsched_dfs_completed_schedules() });
}
