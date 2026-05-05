//! LD_PRELOAD shim for rsched.
//!
//! Build with `cargo build -p rsched-preload` to produce `librsched_preload.so`.
//! Then run a program with:
//!
//!   LD_PRELOAD=/path/to/librsched_preload.so ./my_program
//!
//! Every pthread_create / pthread_join / pthread_mutex_* / pthread_cond_* /
//! pthread_barrier_* / sched_yield call in the target binary is redirected
//! to the rsched cooperative scheduler.
//!
//! This library intentionally exports ONLY the pthread-named symbols.
//! Programs that want static linking should link against librsched.a instead
//! and use the rsched.h header (which #define-redirects calls at compile time).

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use scopeguard::defer;

use libc::{
    c_int, c_uint, c_void,
    pthread_attr_t, pthread_t,
    pthread_mutex_t,
    pthread_cond_t,
    pthread_barrier_t, pthread_barrierattr_t,
};

// Declare the rsched_* symbols via their C ABI.  This avoids any Rust-level
// visibility issues with private type aliases in the rsched crate.
unsafe extern "C" {
    fn rsched_pthread_create(
        thread: *mut pthread_t,
        attr:   *const pthread_attr_t,
        start:  unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        arg:    *mut c_void,
    ) -> c_int;

    fn rsched_pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;
    fn rsched_pthread_exit(retval: *mut c_void) -> !;

    fn rsched_pthread_mutex_lock(m: *mut pthread_mutex_t) -> c_int;
    fn rsched_pthread_mutex_trylock(m: *mut pthread_mutex_t) -> c_int;
    fn rsched_pthread_mutex_unlock(m: *mut pthread_mutex_t) -> c_int;

    fn rsched_pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int;
    fn rsched_pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int;
    fn rsched_pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int;

    fn rsched_pthread_barrier_init(
        barrier: *mut pthread_barrier_t,
        attr:    *const pthread_barrierattr_t,
        count:   c_uint,
    ) -> c_int;
    fn rsched_pthread_barrier_wait(barrier: *mut pthread_barrier_t) -> c_int;

    fn rsched_sched_yield() -> c_int;
}

// ── Reentrancy depth tracking ─────────────────────────────────────────────────
//
// Each exported function increments CALL_DEPTH on entry and decrements it via
// defer! on exit.  A depth > 1 on entry means a recursive (re-entrant) call,
// which indicates a bug — our rsched_* implementations must never call back
// through the LD_PRELOAD symbols.

thread_local! {
    static CALL_DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[inline]
fn enter() {
    CALL_DEPTH.with(|d| {
        let prev = d.get();
        assert_eq!(prev, 0, "rsched preload: re-entrant pthread call detected (depth {})", prev + 1);
        d.set(prev + 1);
    });
}

#[inline]
fn exit() {
    CALL_DEPTH.with(|d| d.set(d.get() - 1));
}

// ── Thread lifecycle ──────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    attr:   *const pthread_attr_t,
    start:  unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    arg:    *mut c_void,
) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_create(thread, attr, start, arg)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(
    thread: pthread_t,
    retval: *mut *mut c_void,
) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_join(thread, retval)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_exit(retval: *mut c_void) -> ! {
    enter();
    // No defer!(exit()) — diverging function never returns.
    rsched_pthread_exit(retval)
}

// ── Mutex ─────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_lock(m: *mut pthread_mutex_t) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_mutex_lock(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_trylock(m: *mut pthread_mutex_t) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_mutex_trylock(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut pthread_mutex_t) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_mutex_unlock(m)
}

// ── Condition variables ───────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond:  *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_cond_wait(cond, mutex)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_cond_signal(cond)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_cond_broadcast(cond)
}

// ── Barriers ──────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_barrier_init(
    barrier: *mut pthread_barrier_t,
    attr:    *const pthread_barrierattr_t,
    count:   c_uint,
) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_barrier_init(barrier, attr, count)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_barrier_wait(
    barrier: *mut pthread_barrier_t,
) -> c_int {
    enter(); defer!(exit());
    rsched_pthread_barrier_wait(barrier)
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_yield() -> c_int {
    enter(); defer!(exit());
    rsched_sched_yield()
}
