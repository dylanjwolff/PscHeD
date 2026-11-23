//! Deterministic environment inputs: time, the current CPU, and randomness.
//!
//! A program that reads the real clock, CPU number, or kernel randomness can
//! take a different path on each run of one seed. Under rsched these come
//! from models that depend only on the schedule and `RANDOM_SEED`:
//!
//! - Time is virtual. `CLOCK_REALTIME` and the monotonic clocks start at
//!   their real values when rsched starts and then advance together by
//!   [`TICK_NS`] at every scheduling point and every clock read, so a loop
//!   that spins on the clock still ends. A timed wait or sleep that expires
//!   moves them to its deadline.
//! - `sched_getcpu` reports a CPU derived from the calling task.
//! - `getrandom` returns bytes from a generator seeded by `RANDOM_SEED`.
//!
//! The state lives in process-shared memory, so time keeps moving forward
//! across `fork`.

use crate::{ensure_init, instrumented_libc_ready, my_pt, rsched_glock, rsched_gunlock, st};
use libc::{c_int, c_long, c_uint, c_void, clockid_t, timespec};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Virtual nanoseconds that pass at each scheduling point and clock read.
pub(crate) const TICK_NS: u64 = 1000;

const NS: i64 = 1_000_000_000;

/// Zero-initialized in process-shared memory.
#[repr(C)]
pub(crate) struct SharedEnv {
    /// Nonzero once the bases below are set.
    started: AtomicU64,
    realtime_base_ns: AtomicI64,
    monotonic_base_ns: AtomicI64,
    /// Virtual nanoseconds since the bases were taken.
    elapsed_ns: AtomicU64,
    random_seed: AtomicU64,
    /// 64-bit words `getrandom` has handed out.
    random_counter: AtomicU64,
}

fn env() -> &'static SharedEnv {
    unsafe { &*crate::task_provider::shared_env() }
}

unsafe fn real_now_ns(clock: clockid_t) -> i64 {
    let mut ts: timespec = std::mem::zeroed();
    crate::seccomp::raw_syscall6(
        libc::SYS_clock_gettime,
        clock as c_long,
        &mut ts as *mut timespec as c_long,
        0,
        0,
        0,
        0,
    );
    ts.tv_sec * NS + ts.tv_nsec
}

/// Takes the clock bases once per process tree; resets the random stream
/// for each execution, so `rsched_reinit` replays the same bytes.
pub(crate) unsafe fn start(seed: u64) {
    let env = env();
    if env.started.load(Ordering::Acquire) == 0 {
        env.realtime_base_ns
            .store(real_now_ns(libc::CLOCK_REALTIME), Ordering::Relaxed);
        env.monotonic_base_ns
            .store(real_now_ns(libc::CLOCK_MONOTONIC), Ordering::Relaxed);
        env.started.store(1, Ordering::Release);
    }
    env.random_seed.store(seed, Ordering::Relaxed);
    env.random_counter.store(0, Ordering::Relaxed);
}

fn started() -> bool {
    instrumented_libc_ready()
        && crate::INITED.load(Ordering::Acquire)
        && env().started.load(Ordering::Acquire) != 0
}

/// The base of `clock`, or None for clocks that are not virtual, such as
/// CPU-time clocks.
fn base_ns(clock: clockid_t) -> Option<i64> {
    let env = env();
    match clock {
        libc::CLOCK_REALTIME | libc::CLOCK_REALTIME_COARSE => {
            Some(env.realtime_base_ns.load(Ordering::Relaxed))
        }
        libc::CLOCK_MONOTONIC
        | libc::CLOCK_MONOTONIC_RAW
        | libc::CLOCK_MONOTONIC_COARSE
        | libc::CLOCK_BOOTTIME => Some(env.monotonic_base_ns.load(Ordering::Relaxed)),
        _ => None,
    }
}

pub(crate) fn is_virtual(clock: clockid_t) -> bool {
    base_ns(clock).is_some()
}

/// Advances time by one tick; called at every scheduling point.
pub(crate) fn tick() {
    if env().started.load(Ordering::Relaxed) != 0 {
        env().elapsed_ns.fetch_add(TICK_NS, Ordering::Relaxed);
    }
}

fn to_timespec(ns: i64) -> timespec {
    timespec {
        tv_sec: ns.div_euclid(NS),
        tv_nsec: ns.rem_euclid(NS),
    }
}

fn to_ns(ts: &timespec) -> i64 {
    ts.tv_sec.saturating_mul(NS).saturating_add(ts.tv_nsec)
}

/// The current time on `clock`, which must be virtual.
pub(crate) fn now(clock: clockid_t) -> timespec {
    let base = base_ns(clock).expect("rsched: clock is not virtual");
    to_timespec(base.saturating_add(env().elapsed_ns.load(Ordering::Relaxed) as i64))
}

/// Moves time forward, if needed, so that `clock` reads at least `at`.
pub(crate) fn advance_to(clock: clockid_t, at: &timespec) {
    let Some(base) = base_ns(clock) else {
        return;
    };
    let target = to_ns(at).saturating_sub(base).max(0) as u64;
    env().elapsed_ns.fetch_max(target, Ordering::Relaxed);
}

/// Nanoseconds from now until `at` on `clock`; zero once it has passed.
pub(crate) fn remaining_ns(clock: clockid_t, at: &timespec) -> i64 {
    (to_ns(at) - to_ns(&now(clock))).max(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_clock_gettime(clock: clockid_t, tp: *mut timespec) -> c_int {
    if !started() || !is_virtual(clock) {
        let r = crate::seccomp::raw_syscall6(
            libc::SYS_clock_gettime,
            clock as c_long,
            tp as c_long,
            0,
            0,
            0,
            0,
        );
        if r < 0 {
            *libc::__errno_location() = -r as c_int;
            return -1;
        }
        return 0;
    }
    if tp.is_null() {
        *libc::__errno_location() = libc::EFAULT;
        return -1;
    }
    env().elapsed_ns.fetch_add(TICK_NS, Ordering::Relaxed);
    *tp = now(clock);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sched_getcpu() -> c_int {
    static CPUS: AtomicU64 = AtomicU64::new(0);
    let mut cpus = CPUS.load(Ordering::Relaxed);
    if cpus == 0 {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let n = if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            libc::CPU_COUNT(&set) as u64
        } else {
            1
        };
        cpus = n.max(1);
        CPUS.store(cpus, Ordering::Relaxed);
    }
    if !started() {
        return libc::sched_getcpu();
    }
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    let task = st()
        .info
        .get(&caller)
        .and_then(|t| t.task)
        .map(|t| t.creation_idx());
    rsched_gunlock();
    match task {
        Some(task) => (task as u64 % cpus) as c_int,
        // A thread rsched does not schedule.
        None => 0,
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Fills `buf` like `getrandom(2)`: returns the byte count, or -errno.
pub(crate) unsafe fn fill_random(buf: *mut c_void, len: usize, flags: c_uint) -> c_long {
    const KNOWN: c_uint = libc::GRND_NONBLOCK | libc::GRND_RANDOM | 0x4 /* GRND_INSECURE */;
    if flags & !KNOWN != 0 {
        return -(libc::EINVAL as c_long);
    }
    // Like the kernel, fill at most 32 MiB - 1 per call.
    let len = len.min((1 << 25) - 1);
    let env = env();
    let seed = env.random_seed.load(Ordering::Relaxed);
    let words = len.div_ceil(8) as u64;
    let first = env.random_counter.fetch_add(words, Ordering::Relaxed);
    let out = buf.cast::<u8>();
    for i in 0..words {
        let word = splitmix64(seed ^ splitmix64(first + i)).to_le_bytes();
        let at = (i * 8) as usize;
        let n = (len - at).min(8);
        std::ptr::copy_nonoverlapping(word.as_ptr(), out.add(at), n);
    }
    len as c_long
}

/// Whether `getrandom` should come from the model.
pub(crate) fn models_random() -> bool {
    started()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_getrandom(buf: *mut c_void, len: usize, flags: c_uint) -> isize {
    let r = if models_random() {
        fill_random(buf, len, flags)
    } else {
        crate::seccomp::raw_syscall6(
            libc::SYS_getrandom,
            buf as c_long,
            len as c_long,
            flags as c_long,
            0,
            0,
            0,
        )
    };
    if r < 0 {
        *libc::__errno_location() = -r as c_int;
        return -1;
    }
    r as isize
}
