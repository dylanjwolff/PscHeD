//! Deterministic environment inputs: time, the current CPU, and randomness.
//! File times, which follow virtual time, are kept in `files`.
//!
//! A program that reads the real clock, CPU number, or kernel randomness can
//! take a different path on each run of one seed. Under rsched these come
//! from models that depend only on the schedule and `RANDOM_SEED`:
//!
//! - Time is virtual. `CLOCK_REALTIME` and the monotonic clocks start at
//!   their real values when rsched starts and then advance together by
//!   [`TICK_NS`] at every scheduling point and every clock read, so a loop
//!   that spins on the clock still ends. A timed wait or sleep that expires
//!   moves them to its deadline, and a `poll` timeout rsched expires moves
//!   them by the timeout. A syscall that rsched does not model and that may
//!   wait in the kernel, such as `ppoll` or `epoll_wait`, moves them by the
//!   real time it blocked, so a timeout never passes while the clock stands
//!   still; only such real waits make time depend on more than the schedule.
//!   libc's `ppoll`, `select`, `pselect`, `epoll_wait`, `epoll_pwait`, and
//!   `sigtimedwait` are routed here too, since they make their syscalls
//!   inline.
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

pub(crate) fn started() -> bool {
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

/// The real time at which virtual `CLOCK_REALTIME` started.
pub(crate) fn realtime_base_ns() -> i64 {
    env().realtime_base_ns.load(Ordering::Relaxed)
}

/// The current virtual `CLOCK_REALTIME`.
pub(crate) fn realtime_now_ns() -> i64 {
    to_ns(&now(libc::CLOCK_REALTIME))
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

/// Moves time forward by `ns`.
pub(crate) fn advance_by(ns: i64) {
    if started() && ns > 0 {
        env().elapsed_ns.fetch_add(ns as u64, Ordering::Relaxed);
    }
}

/// Whether syscall `nr`, unmodeled, may wait in the kernel for a timeout.
pub(crate) fn waits_in_kernel(nr: c_long) -> bool {
    const SYS_EPOLL_PWAIT2: c_long = 441;
    matches!(
        nr,
        libc::SYS_ppoll
            | libc::SYS_select
            | libc::SYS_pselect6
            | libc::SYS_epoll_wait
            | libc::SYS_epoll_pwait
            | SYS_EPOLL_PWAIT2
            | libc::SYS_nanosleep
            | libc::SYS_clock_nanosleep
            | libc::SYS_rt_sigtimedwait
    )
}

/// Runs `call`, a syscall that may wait in the kernel, and moves virtual
/// time forward by the real time it took.
pub(crate) unsafe fn kernel_wait(call: impl FnOnce() -> c_long) -> c_long {
    if !started() {
        return call();
    }
    let start = real_now_ns(libc::CLOCK_MONOTONIC);
    let result = call();
    advance_by(real_now_ns(libc::CLOCK_MONOTONIC) - start);
    result
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
pub unsafe extern "C" fn rsched_time(t: *mut libc::time_t) -> libc::time_t {
    let mut ts: timespec = std::mem::zeroed();
    rsched_clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    if !t.is_null() {
        *t = ts.tv_sec;
    }
    ts.tv_sec
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_gettimeofday(tv: *mut libc::timeval, tz: *mut c_void) -> c_int {
    if !tz.is_null() {
        crate::seccomp::raw_syscall6(libc::SYS_gettimeofday, 0, tz as c_long, 0, 0, 0, 0);
    }
    if !tv.is_null() {
        let mut ts: timespec = std::mem::zeroed();
        rsched_clock_gettime(libc::CLOCK_REALTIME, &mut ts);
        *tv = libc::timeval {
            tv_sec: ts.tv_sec,
            tv_usec: ts.tv_nsec / 1000,
        };
    }
    0
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

/// Entry points for libc calls that may wait in the kernel for a timeout.
/// libc makes their syscalls inline, so `syscall` interception never sees
/// them. Each runs the real call and moves virtual time by the time it took.
macro_rules! kernel_wait_entry {
    ($name:ident, $real:ident, ($($arg:ident: $ty:ty),*)) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> c_int {
            kernel_wait(|| crate::with_internal_depth(|| libc::$real($($arg),*)) as c_long) as c_int
        }
    };
}

kernel_wait_entry!(rsched_ppoll, ppoll, (
    fds: *mut libc::pollfd,
    nfds: libc::nfds_t,
    timeout: *const timespec,
    sigmask: *const libc::sigset_t
));
kernel_wait_entry!(rsched_select, select, (
    nfds: c_int,
    readfds: *mut libc::fd_set,
    writefds: *mut libc::fd_set,
    exceptfds: *mut libc::fd_set,
    timeout: *mut libc::timeval
));
kernel_wait_entry!(rsched_pselect, pselect, (
    nfds: c_int,
    readfds: *mut libc::fd_set,
    writefds: *mut libc::fd_set,
    exceptfds: *mut libc::fd_set,
    timeout: *const timespec,
    sigmask: *const libc::sigset_t
));
kernel_wait_entry!(rsched_epoll_wait, epoll_wait, (
    epfd: c_int,
    events: *mut libc::epoll_event,
    maxevents: c_int,
    timeout: c_int
));
kernel_wait_entry!(rsched_epoll_pwait, epoll_pwait, (
    epfd: c_int,
    events: *mut libc::epoll_event,
    maxevents: c_int,
    timeout: c_int,
    sigmask: *const libc::sigset_t
));
kernel_wait_entry!(rsched_sigtimedwait, sigtimedwait, (
    set: *const libc::sigset_t,
    info: *mut libc::siginfo_t,
    timeout: *const timespec
));
