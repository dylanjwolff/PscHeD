//! Deterministic environment inputs: time, the current CPU, and randomness.
//! File times, which follow virtual time, are kept in `files`.
//!
//! A program that reads the real clock, CPU number, or kernel randomness can
//! take a different path on each run of one seed. Under rsched these come
//! from models that depend only on the schedule and `RANDOM_SEED`:
//!
//! - Time is virtual. `CLOCK_REALTIME` and the monotonic clocks start at
//!   fixed values, [`REALTIME_BASE_NS`] and [`MONOTONIC_BASE_NS`], and then
//!   advance together by [`TICK_NS`] at every scheduling point and every
//!   clock read, so a loop that spins on the clock still ends. A timed wait
//!   or sleep that expires moves them to its deadline, and a `poll` timeout
//!   rsched expires moves them by the timeout. A syscall that rsched does not
//!   model and that may wait in the kernel, such as `ppoll` or `epoll_wait`,
//!   moves them by one tick, and by its timeout when it reports one, so a
//!   timeout never passes while the clock stands still. `epoll_wait` and
//!   `epoll_pwait` are modeled instead, like `poll` on the epoll descriptor. libc's `ppoll`,
//!   `select`, `pselect`, `epoll_wait`, `epoll_pwait`, and `sigtimedwait` are
//!   routed here too, since they make their syscalls inline.
//! - `sched_getcpu` reports a CPU derived from the calling task.
//! - `getrandom` returns bytes from a generator seeded by `RANDOM_SEED`.
//!
//! The state lives in process-shared memory, so time keeps moving forward
//! across `fork`.

use crate::{ensure_init, instrumented_libc_ready, my_pt, rsched_glock, rsched_gunlock, st};
use libc::{c_int, c_long, c_uint, c_void, clockid_t, timespec};
use std::sync::atomic::{AtomicU64, Ordering};

/// Virtual nanoseconds that pass at each scheduling point and clock read.
pub(crate) const TICK_NS: u64 = 1000;

const NS: i64 = 1_000_000_000;

/// Where virtual `CLOCK_REALTIME` starts: 2024-01-01T00:00:00Z.
pub(crate) const REALTIME_BASE_NS: i64 = 1_704_067_200 * NS;

/// Where the virtual monotonic and boot-time clocks start.
const MONOTONIC_BASE_NS: i64 = 1_000 * NS;

/// Zero-initialized in process-shared memory.
#[repr(C)]
pub(crate) struct SharedEnv {
    /// Nonzero once rsched has started.
    started: AtomicU64,
    /// Virtual nanoseconds since rsched started.
    elapsed_ns: AtomicU64,
    random_seed: AtomicU64,
    /// 64-bit words `getrandom` has handed out.
    random_counter: AtomicU64,
}

fn env() -> &'static SharedEnv {
    unsafe { &*crate::task_provider::shared_env() }
}

/// Starts the clocks once per process tree; resets the random stream for
/// each execution, so `rsched_reinit` replays the same bytes.
pub(crate) unsafe fn start(seed: u64) {
    let env = env();
    env.started.store(1, Ordering::Release);
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
    match clock {
        libc::CLOCK_REALTIME | libc::CLOCK_REALTIME_COARSE => Some(REALTIME_BASE_NS),
        libc::CLOCK_MONOTONIC
        | libc::CLOCK_MONOTONIC_RAW
        | libc::CLOCK_MONOTONIC_COARSE
        | libc::CLOCK_BOOTTIME => Some(MONOTONIC_BASE_NS),
        _ => None,
    }
}

pub(crate) fn is_virtual(clock: clockid_t) -> bool {
    base_ns(clock).is_some()
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
            | libc::SYS_rt_sigtimedwait
    )
}

/// The timeout of `nr`, one of the [`waits_in_kernel`] syscalls, called
/// with `args`; None when it may wait forever.
unsafe fn wait_timeout_ns(nr: c_long, args: &[c_long; 6]) -> Option<i64> {
    const SYS_EPOLL_PWAIT2: c_long = 441;
    let ts = |p: c_long| (p != 0).then(|| to_ns(&*(p as *const timespec)));
    let ms = |t: c_long| (t as c_int >= 0).then(|| i64::from(t as c_int) * 1_000_000);
    match nr {
        libc::SYS_ppoll | libc::SYS_rt_sigtimedwait => ts(args[2]),
        libc::SYS_pselect6 => ts(args[4]),
        SYS_EPOLL_PWAIT2 => ts(args[3]),
        libc::SYS_select => (args[4] != 0).then(|| {
            let tv = &*(args[4] as *const libc::timeval);
            tv.tv_sec
                .saturating_mul(NS)
                .saturating_add(tv.tv_usec * 1000)
        }),
        libc::SYS_epoll_wait | libc::SYS_epoll_pwait => ms(args[3]),
        _ => None,
    }
}

/// Whether `nr`, one of the [`waits_in_kernel`] syscalls, called with
/// `args`, watches no descriptor and so can only time out.
fn waits_on_nothing(nr: c_long, args: &[c_long; 6]) -> bool {
    match nr {
        libc::SYS_select | libc::SYS_pselect6 => {
            args[0] == 0 || (args[1] == 0 && args[2] == 0 && args[3] == 0)
        }
        libc::SYS_ppoll => args[1] == 0,
        _ => false,
    }
}

/// Writes a zero time left into the timeout of a `select` or `ppoll` that
/// timed out, as Linux does.
unsafe fn clear_time_left(nr: c_long, args: &[c_long; 6]) {
    match nr {
        libc::SYS_select if args[4] != 0 => {
            *(args[4] as *mut libc::timeval) = libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            };
        }
        libc::SYS_ppoll if args[2] != 0 => *(args[2] as *mut timespec) = to_timespec(0),
        _ => {}
    }
}

/// Runs `call`, syscall `nr` with `args`, which may wait in the kernel and
/// returns as a libc function does (-1 and errno on failure). Time moves by
/// one tick, and by the call's timeout when it reports that the timeout
/// passed, so it never depends on how long the call really blocked.
pub(crate) unsafe fn kernel_wait(
    nr: c_long,
    args: [c_long; 6],
    call: impl FnOnce() -> c_long,
) -> c_long {
    if !started() {
        return call();
    }
    if nr == libc::SYS_epoll_wait || nr == libc::SYS_epoll_pwait {
        // Modeled: wait for an event as a scheduling point, then collect it
        // with a zero timeout.
        let mut args = args;
        args[3] = crate::io::prepare_epoll_wait(
            crate::return_address(),
            args[0] as c_int,
            args[3] as c_int,
        )
        .into();
        if nr == libc::SYS_epoll_pwait && args[4] != 0 {
            // The libc entry point passes no sigset size; the kernel's is 8.
            args[5] = 8;
        }
        let r = crate::seccomp::raw_syscall6(nr, args[0], args[1], args[2], args[3], args[4], args[5]);
        return crate::io::libc_result(r);
    }
    // Read first: Linux's select writes the time left into its timeval.
    let timeout = wait_timeout_ns(nr, &args);
    // A wait on no descriptor is a sleep: no task can end it early, so it
    // sleeps in virtual time, letting the other tasks run, and reports the
    // timeout. Waiting in the kernel would take real time and, with no
    // scheduling point, starve every other task.
    if let Some(ns) = timeout
        && waits_on_nothing(nr, &args)
    {
        crate::sync::sleep_for(ns, crate::return_address());
        clear_time_left(nr, &args);
        return 0;
    }
    let result = call();
    let timed_out = if nr == libc::SYS_rt_sigtimedwait {
        result == -1 && *libc::__errno_location() == libc::EAGAIN
    } else {
        result == 0
    };
    advance_by(TICK_NS as i64);
    if timed_out && let Some(ns) = timeout {
        advance_by(ns);
    }
    result
}

/// The elapsed virtual time at which `clock`, which must be virtual,
/// reads `at`.
pub(crate) fn elapsed_at(clock: clockid_t, at: &timespec) -> u64 {
    let base = base_ns(clock).expect("rsched: clock is not virtual");
    to_ns(at).saturating_sub(base).max(0) as u64
}

/// The elapsed virtual time.
pub(crate) fn elapsed() -> u64 {
    env().elapsed_ns.load(Ordering::Relaxed)
}

/// Moves time forward, if needed, so that `clock` reads at least `at`.
pub(crate) fn advance_to(clock: clockid_t, at: &timespec) {
    if is_virtual(clock) {
        advance_to_elapsed(elapsed_at(clock, at));
    }
}

/// Moves time forward, if needed, to elapsed time `target`.
pub(crate) fn advance_to_elapsed(target: u64) {
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
/// them. Each runs the real call through [`kernel_wait`].
macro_rules! kernel_wait_entry {
    ($name:ident, $real:ident, $nr:expr, ($($arg:ident: $ty:ty),*)) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> c_int {
            let mut args = [0; 6];
            for (slot, arg) in args.iter_mut().zip([$($arg as c_long),*]) {
                *slot = arg;
            }
            let call = || crate::with_internal_depth(|| libc::$real($($arg),*)) as c_long;
            kernel_wait($nr, args, call) as c_int
        }
    };
}

kernel_wait_entry!(rsched_ppoll, ppoll, libc::SYS_ppoll, (
    fds: *mut libc::pollfd,
    nfds: libc::nfds_t,
    timeout: *const timespec,
    sigmask: *const libc::sigset_t
));
kernel_wait_entry!(rsched_select, select, libc::SYS_select, (
    nfds: c_int,
    readfds: *mut libc::fd_set,
    writefds: *mut libc::fd_set,
    exceptfds: *mut libc::fd_set,
    timeout: *mut libc::timeval
));
kernel_wait_entry!(rsched_pselect, pselect, libc::SYS_pselect6, (
    nfds: c_int,
    readfds: *mut libc::fd_set,
    writefds: *mut libc::fd_set,
    exceptfds: *mut libc::fd_set,
    timeout: *const timespec,
    sigmask: *const libc::sigset_t
));
kernel_wait_entry!(rsched_epoll_wait, epoll_wait, libc::SYS_epoll_wait, (
    epfd: c_int,
    events: *mut libc::epoll_event,
    maxevents: c_int,
    timeout: c_int
));
kernel_wait_entry!(rsched_epoll_pwait, epoll_pwait, libc::SYS_epoll_pwait, (
    epfd: c_int,
    events: *mut libc::epoll_event,
    maxevents: c_int,
    timeout: c_int,
    sigmask: *const libc::sigset_t
));
kernel_wait_entry!(rsched_sigtimedwait, sigtimedwait, libc::SYS_rt_sigtimedwait, (
    set: *const libc::sigset_t,
    info: *mut libc::siginfo_t,
    timeout: *const timespec
));
