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
//!
//! Reentrancy handling: each exported function increments CALL_DEPTH on entry.
//! If depth > 0 on entry (i.e., we are already inside rsched), the call is a
//! re-entrant call (e.g. from TSAN's internal background thread, or from rsched's
//! own internals via PLT).  In that case we bypass rsched entirely and forward
//! directly to the next implementation via dlsym(RTLD_NEXT, …).

#![allow(unsafe_op_in_unsafe_fn)]

use std::sync::atomic::{AtomicUsize, Ordering};
use scopeguard::defer;

use libc::{
    c_int, c_uint, c_void,
    pthread_attr_t, pthread_t,
    pthread_mutex_t,
    pthread_cond_t,
    pthread_barrier_t, pthread_barrierattr_t,
    RTLD_NEXT,
};

use rsched::{
    rsched_pthread_create,
    rsched_pthread_join,
    rsched_pthread_exit,
    rsched_pthread_mutex_lock,
    rsched_pthread_mutex_trylock,
    rsched_pthread_mutex_unlock,
    rsched_pthread_cond_wait,
    rsched_pthread_cond_signal,
    rsched_pthread_cond_broadcast,
    rsched_pthread_barrier_init,
    rsched_pthread_barrier_wait,
    rsched_sched_yield,
    rsched_try_enter,
    rsched_exit,
};

// ── Reentrancy depth tracking ─────────────────────────────────────────────────
//
// CALL_DEPTH lives in rsched (the rlib linked into this DSO).  Each interceptor
// calls rsched_try_enter() on entry and rsched_exit() on exit via defer!().
// rsched's own trampoline() and do_thread_exit() call depth_enter/exit() directly
// on the same TLS slot, so all re-entrancy checks are consistent.

// ── RTLD_NEXT resolution diagnostic ──────────────────────────────────────────

/// Print where `dlsym(RTLD_NEXT, name)` resolves for each intercepted symbol.
/// Call this from a test binary (linked against librsched_preload.so) to
/// verify whether RTLD_NEXT lands in the real libc or in an ASAN interceptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_diagnose_rtld_next() {
    let names: &[&[u8]] = &[
        b"pthread_create\0",
        b"pthread_join\0",
        b"pthread_mutex_lock\0",
        b"pthread_mutex_unlock\0",
        b"pthread_cond_wait\0",
        b"pthread_cond_signal\0",
        b"pthread_cond_broadcast\0",
        b"pthread_barrier_init\0",
        b"pthread_barrier_wait\0",
        b"sched_yield\0",
    ];
    eprintln!("[rsched] RTLD_NEXT resolutions from librsched_preload.so:");
    for name in names {
        let sym_name = core::str::from_utf8(&name[..name.len()-1]).unwrap_or("?");
        let p = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const _);
        if p.is_null() {
            eprintln!("  {:32} → NULL", sym_name);
            continue;
        }
        let mut info: libc::Dl_info = core::mem::zeroed();
        if libc::dladdr(p, &mut info) != 0 && !info.dli_fname.is_null() {
            let fname = core::ffi::CStr::from_ptr(info.dli_fname)
                .to_str()
                .unwrap_or("?");
            eprintln!("  {:32} → {:p}  ({})", sym_name, p, fname);
        } else {
            eprintln!("  {:32} → {:p}  (dladdr failed)", sym_name, p);
        }
    }
}

// ── RTLD_NEXT fallback — lock-free per-symbol slots ───────────────────────────
//
// We store each RTLD_NEXT function pointer in a plain AtomicUsize (0 = not yet
// resolved).  On first use, dlsym(RTLD_NEXT, name) is called and the result is
// stored.  No OnceLock / mutex is used, which avoids a deadlock where dlsym
// itself calls pthread_mutex_lock → our interceptor → this initialiser → dlsym
// (OnceLock in RUNNING state → single-thread futex deadlock).

static NEXT_PTHREAD_CREATE:         AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_JOIN:           AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_LOCK:     AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_TRYLOCK:  AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_UNLOCK:   AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_COND_WAIT:      AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_COND_SIGNAL:    AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_COND_BROADCAST: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_BARRIER_INIT:   AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_BARRIER_WAIT:   AtomicUsize = AtomicUsize::new(0);
static NEXT_SCHED_YIELD:            AtomicUsize = AtomicUsize::new(0);

/// Load (and lazily resolve) a single RTLD_NEXT function pointer.
/// Safe to call re-entrantly on a single thread: worst case two threads both
/// call dlsym and the second store overwrites the first with the same value.
#[inline]
unsafe fn load_next<T: Copy>(slot: &AtomicUsize, name: &[u8]) -> T {
    let mut p = slot.load(Ordering::Acquire);
    if p == 0 {
        p = libc::dlsym(RTLD_NEXT, name.as_ptr() as *const _) as usize;
        assert!(p != 0,
            "rsched preload: dlsym(RTLD_NEXT, {:?}) returned null",
            core::str::from_utf8(name).unwrap_or("?"));
        slot.store(p, Ordering::Release);
    }
    std::mem::transmute_copy::<usize, T>(&p)
}

// ── Thread lifecycle ──────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    attr:   *const pthread_attr_t,
    start:  unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    arg:    *mut c_void,
) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_t, *const pthread_attr_t,
                                    unsafe extern "C" fn(*mut c_void) -> *mut c_void,
                                    *mut c_void) -> c_int =
            load_next(&NEXT_PTHREAD_CREATE, b"pthread_create\0");
        return f(thread, attr, start, arg);
    }
    rsched_pthread_create(thread, attr, start, arg)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(
    thread: pthread_t,
    retval: *mut *mut c_void,
) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(pthread_t, *mut *mut c_void) -> c_int =
            load_next(&NEXT_PTHREAD_JOIN, b"pthread_join\0");
        return f(thread, retval);
    }
    rsched_pthread_join(thread, retval)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_exit(retval: *mut c_void) -> ! {
    // diverging: no defer!(exit()) — always route through rsched which never
    // returns, so there is no exit to balance.
    rsched_try_enter();
    rsched_pthread_exit(retval)
}

// ── Mutex ─────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_lock(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_MUTEX_LOCK, b"pthread_mutex_lock\0");
        return f(m);
    }
    rsched_pthread_mutex_lock(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_trylock(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_MUTEX_TRYLOCK, b"pthread_mutex_trylock\0");
        return f(m);
    }
    rsched_pthread_mutex_trylock(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_MUTEX_UNLOCK, b"pthread_mutex_unlock\0");
        return f(m);
    }
    rsched_pthread_mutex_unlock(m)
}

// ── Condition variables ───────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond:  *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_cond_t, *mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_COND_WAIT, b"pthread_cond_wait\0");
        return f(cond, mutex);
    }
    rsched_pthread_cond_wait(cond, mutex)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_cond_t) -> c_int =
            load_next(&NEXT_PTHREAD_COND_SIGNAL, b"pthread_cond_signal\0");
        return f(cond);
    }
    rsched_pthread_cond_signal(cond)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_cond_t) -> c_int =
            load_next(&NEXT_PTHREAD_COND_BROADCAST, b"pthread_cond_broadcast\0");
        return f(cond);
    }
    rsched_pthread_cond_broadcast(cond)
}

// ── Barriers ──────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_barrier_init(
    barrier: *mut pthread_barrier_t,
    attr:    *const pthread_barrierattr_t,
    count:   c_uint,
) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_barrier_t,
                                    *const pthread_barrierattr_t, c_uint) -> c_int =
            load_next(&NEXT_PTHREAD_BARRIER_INIT, b"pthread_barrier_init\0");
        return f(barrier, attr, count);
    }
    rsched_pthread_barrier_init(barrier, attr, count)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_barrier_wait(
    barrier: *mut pthread_barrier_t,
) -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_barrier_t) -> c_int =
            load_next(&NEXT_PTHREAD_BARRIER_WAIT, b"pthread_barrier_wait\0");
        return f(barrier);
    }
    rsched_pthread_barrier_wait(barrier)
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_yield() -> c_int {
    let outermost = rsched_try_enter(); defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn() -> c_int =
            load_next(&NEXT_SCHED_YIELD, b"sched_yield\0");
        return f();
    }
    rsched_sched_yield()
}
