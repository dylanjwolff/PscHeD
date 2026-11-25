//! Models of blocking primitives beyond mutexes and untimed condition waits:
//! semaphores, timed condition waits, rwlocks, `pthread_once`, and sleeps.
//!
//! Timed waits run in virtual time (see `virt`). A wait expires once virtual
//! time reaches its deadline, which every scheduling point moves closer, so a
//! spinning task cannot starve it forever. When no task can run, the
//! scheduler picks one timed wait and moves virtual time to its deadline. Both
//! depend only on the schedule, and a timeout is never reported before its
//! deadline has passed.

use crate::event::{Event, EventKind};
use crate::scheduler::logging_enabled;
use crate::{task_provider, virt};
use crate::{
    CondT, PthreadT, State, ensure_init, log_line, my_pt, return_address, rsched_glock,
    rsched_gunlock, seccomp, st, with_internal_depth,
};
use libc::{c_int, c_long, c_uint, c_void, clockid_t, timespec};
use std::sync::atomic::{AtomicI32, Ordering};

#[derive(Clone, Copy)]
pub(crate) struct Deadline {
    clock: clockid_t,
    at: timespec,
}

/// What a blocked task waits on, so an expiring wait can leave its queue.
#[derive(Clone, Copy)]
pub(crate) enum WaitOn {
    Sem(usize),
    Cond(usize),
    RwLock(usize),
    Once,
    Sleep,
}

#[derive(Clone, Copy)]
pub(crate) struct TimedWait {
    pthread: PthreadT,
    on: WaitOn,
    deadline: Deadline,
}

/// A reader-writer lock. The real `pthread_rwlock_t` is never locked: every
/// public rwlock operation goes through this model.
#[derive(Default)]
pub(crate) struct SRwLock {
    writer: libc::pid_t,
    /// One entry per read lock held; a thread may hold several.
    readers: Vec<libc::pid_t>,
    /// Blocked lockers. An unlock wakes all of them to try again.
    waiters: Vec<PthreadT>,
}

fn valid(ts: &timespec) -> bool {
    ts.tv_nsec >= 0 && ts.tv_nsec < 1_000_000_000
}

fn supported_clock(clock: clockid_t) -> bool {
    clock == libc::CLOCK_REALTIME || clock == libc::CLOCK_MONOTONIC
}

/// The deadline `rel` after now on `clock`, which must be virtual.
pub(crate) unsafe fn after(clock: clockid_t, rel: timespec) -> Deadline {
    let now = virt::now(clock);
    let mut at = timespec {
        tv_sec: now.tv_sec.saturating_add(rel.tv_sec),
        tv_nsec: now.tv_nsec + rel.tv_nsec,
    };
    if at.tv_nsec >= 1_000_000_000 {
        at.tv_nsec -= 1_000_000_000;
        at.tv_sec = at.tv_sec.saturating_add(1);
    }
    Deadline { clock, at }
}

unsafe fn errno() -> c_int {
    *libc::__errno_location()
}

unsafe fn set_errno(value: c_int) {
    *libc::__errno_location() = value;
}

impl State {
    /// Blocks `caller` until another task makes it runnable or, with a
    /// deadline, until the wait expires. Returns whether it expired. The
    /// caller has already queued itself on the object named by `on`.
    pub(crate) unsafe fn block_on(
        &mut self,
        caller: PthreadT,
        on: WaitOn,
        deadline: Option<Deadline>,
    ) -> bool {
        self.t(caller).timed_out = false;
        let sleeping = matches!(on, WaitOn::Sleep);
        // A signal sent before the task got here interrupts a sleep at once.
        if sleeping && crate::signals::pending() {
            self.task(caller).set_interrupted();
            return false;
        }
        if let Some(deadline) = deadline {
            self.timed_waits.push(TimedWait {
                pthread: caller,
                on,
                deadline,
            });
            self.task(caller)
                .set_deadline(virt::elapsed_at(deadline.clock, &deadline.at));
        }
        if sleeping {
            self.task(caller).set_sleeping(true);
        }
        self.task(caller).set_blocking(true);
        self.context_switch(caller);
        self.park_if_blocked(caller);
        if sleeping {
            self.task(caller).set_sleeping(false);
        }
        self.reap_remote_expiries();
        self.timed_waits.retain(|w| w.pthread != caller);
        self.task(caller).set_deadline(task_provider::NO_DEADLINE);
        std::mem::take(&mut self.t(caller).timed_out)
    }

    /// Finishes the timed waits of this process's tasks that another
    /// process expired: they leave the queues they waited on and see the
    /// timeout. Runs whenever a task of this process resumes, so no task
    /// here wakes a waiter whose wait already expired.
    pub(crate) unsafe fn reap_remote_expiries(&mut self) {
        let waits = self.timed_waits.clone();
        for wait in waits {
            if self.threads.contains(&wait.pthread) && self.task(wait.pthread).take_timed_out() {
                self.expire(wait, "remote");
            }
        }
    }

    /// Expires every timed wait whose deadline virtual time has reached.
    pub(crate) unsafe fn expire_due_waits(&mut self) {
        let due: Vec<TimedWait> = self
            .timed_waits
            .iter()
            .filter(|w| {
                self.threads.contains(&w.pthread)
                    && self.task(w.pthread).is_blocking()
                    && virt::remaining_ns(w.deadline.clock, &w.deadline.at) == 0
            })
            .copied()
            .collect();
        for wait in due {
            self.expire(wait, "due");
        }
        let now = virt::elapsed();
        for (task, deadline) in task_provider::remote_timed_waits() {
            if deadline <= now {
                task.expire_remote();
            }
        }
    }

    /// When no task can run: expires the timed wait with the earliest
    /// deadline, chosen by the scheduler among those that tie, and moves
    /// virtual time to its deadline. A later deadline never expires first,
    /// as in real time. Returns whether a task may have become runnable.
    pub(crate) unsafe fn expire_timed_wait(&mut self) -> bool {
        let candidates: Vec<TimedWait> = self
            .timed_waits
            .iter()
            .filter(|w| self.threads.contains(&w.pthread) && self.task(w.pthread).is_blocking())
            .copied()
            .collect();
        // Tasks of other processes too: only one process runs, and its tasks
        // may all wait on I/O that never comes while another process sleeps.
        let remote = task_provider::remote_timed_waits();
        if candidates.is_empty() && remote.is_empty() {
            return false;
        }
        let deadlines: Vec<u64> = candidates
            .iter()
            .map(|w| virt::elapsed_at(w.deadline.clock, &w.deadline.at))
            .chain(remote.iter().map(|&(_, deadline)| deadline))
            .collect();
        let earliest = *deadlines.iter().min().expect("rsched: non-empty timed wait list");
        let ties: Vec<usize> = (0..deadlines.len())
            .filter(|&i| deadlines[i] == earliest)
            .collect();
        let blocking = vec![false; ties.len()];
        let pick = ties[self
            .choose_index(&blocking)
            .expect("rsched: non-empty timed wait list")];
        let Some(&wait) = candidates.get(pick) else {
            let (task, deadline) = remote[pick - candidates.len()];
            virt::advance_to_elapsed(deadline);
            task.expire_remote();
            if logging_enabled() {
                log_line!(
                    "[rsched] TimedWaitExpired(idle, other process) task={}",
                    task.creation_idx()
                );
            }
            return true;
        };
        virt::advance_to(wait.deadline.clock, &wait.deadline.at);
        self.expire(wait, "idle");
        true
    }

    /// Wakes the task of `wait` as timed out.
    unsafe fn expire(&mut self, wait: TimedWait, why: &str) {
        let pt = wait.pthread;
        match wait.on {
            WaitOn::Sem(key) => {
                if let Some(waiters) = self.sem_waiters.get_mut(&key) {
                    waiters.retain(|&w| w != pt);
                }
            }
            WaitOn::Cond(key) => {
                if let Some(cond) = self.conds.get_mut(&key) {
                    cond.waiters.retain(|&w| w != pt);
                }
            }
            WaitOn::RwLock(key) => {
                if let Some(lock) = self.rwlocks.get_mut(&key) {
                    lock.waiters.retain(|&w| w != pt);
                }
            }
            WaitOn::Once | WaitOn::Sleep => {}
        }
        self.t(pt).timed_out = true;
        self.task(pt).set_blocking(false);
        self.task(pt).set_deadline(task_provider::NO_DEADLINE);
        if logging_enabled() {
            log_line!(
                "[rsched] TimedWaitExpired({why}) task={}",
                self.task(pt).creation_idx()
            );
        }
    }

    fn wake_all(&mut self, waiters: Vec<PthreadT>) {
        for w in waiters {
            unsafe { self.task(w).set_blocking(false) };
        }
    }
}

/// A scheduling point for `op` on `obj`, reached with the global lock held.
unsafe fn sync_point(caller: PthreadT, op: &'static str, obj: *const c_void, instr_addr: u64) {
    st().set_event(
        caller,
        Event {
            instr_addr,
            kind: EventKind::Sync { op, obj },
        },
    );
    st().context_switch(caller);
    st().clear_event(caller);
}

/// `kill` as a scheduling point. Must be called with GMTX held.
pub(crate) unsafe fn kill_point(instr_addr: u64) {
    let caller = my_pt();
    if st().threads.contains(&caller) {
        sync_point(caller, "kill", std::ptr::null(), instr_addr);
    }
}

// ── Semaphores ────────────────────────────────────────────────────────────
//
// The real semaphore keeps the count, so operations rsched does not wrap,
// such as sem_getvalue, stay consistent. A waiter that finds it zero blocks
// in the model instead of the kernel; every post wakes all waiters to retry
// sem_trywait. Waiters and posters must be in one process.

unsafe fn sem_wait_impl(
    sem: *mut libc::sem_t,
    deadline: Option<Deadline>,
    op: &'static str,
    instr_addr: u64,
) -> c_int {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    sync_point(caller, op, sem.cast(), instr_addr);
    let key = sem as usize;
    let error = loop {
        if with_internal_depth(|| libc::sem_trywait(sem)) == 0 {
            rsched_gunlock();
            return 0;
        }
        let e = errno();
        if e != libc::EAGAIN {
            break e;
        }
        if deadline.is_some_and(|d| !valid(&d.at)) {
            break libc::EINVAL;
        }
        st().sem_waiters.entry(key).or_default().push(caller);
        if st().block_on(caller, WaitOn::Sem(key), deadline) {
            break libc::ETIMEDOUT;
        }
    };
    rsched_gunlock();
    set_errno(error);
    -1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sem_wait(sem: *mut libc::sem_t) -> c_int {
    sem_wait_impl(sem, None, "sem_wait", return_address())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sem_timedwait(
    sem: *mut libc::sem_t,
    abstime: *const timespec,
) -> c_int {
    // musl's own sem_wait passes a null deadline.
    let deadline = (!abstime.is_null()).then(|| Deadline {
        clock: libc::CLOCK_REALTIME,
        at: *abstime,
    });
    sem_wait_impl(sem, deadline, "sem_timedwait", return_address())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sem_clockwait(
    sem: *mut libc::sem_t,
    clock: clockid_t,
    abstime: *const timespec,
) -> c_int {
    if !supported_clock(clock) {
        set_errno(libc::EINVAL);
        return -1;
    }
    let deadline = Deadline {
        clock,
        at: *abstime,
    };
    sem_wait_impl(sem, Some(deadline), "sem_clockwait", return_address())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sem_trywait(sem: *mut libc::sem_t) -> c_int {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    sync_point(caller, "sem_trywait", sem.cast(), return_address());
    let r = with_internal_depth(|| libc::sem_trywait(sem));
    let e = errno();
    rsched_gunlock();
    if r != 0 {
        set_errno(e);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sem_post(sem: *mut libc::sem_t) -> c_int {
    ensure_init();
    let instr_addr = return_address();
    rsched_glock();
    let caller = my_pt();
    let r = with_internal_depth(|| libc::sem_post(sem));
    let e = errno();
    if r == 0
        && let Some(waiters) = st().sem_waiters.remove(&(sem as usize))
    {
        st().wake_all(waiters);
    }
    sync_point(caller, "sem_post", sem.cast(), instr_addr);
    rsched_gunlock();
    if r != 0 {
        set_errno(e);
    }
    r
}

// ── Timed condition waits ─────────────────────────────────────────────────

/// The clock `pthread_cond_timedwait` measures `cond`'s deadline against,
/// as set by pthread_condattr_setclock.
unsafe fn cond_clock(cond: *const CondT) -> clockid_t {
    #[cfg(target_env = "musl")]
    let monotonic = (cond as *const c_int).add(4).read_volatile() == libc::CLOCK_MONOTONIC;
    // glibc keeps it in __wrefs (offset 36) as __PTHREAD_COND_CLOCK_MONOTONIC_MASK.
    #[cfg(not(target_env = "musl"))]
    let monotonic = (cond as *const u8).add(36).cast::<u32>().read_volatile() & 2 != 0;
    if monotonic {
        libc::CLOCK_MONOTONIC
    } else {
        libc::CLOCK_REALTIME
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_timedwait(
    cond: *mut CondT,
    lock: *mut crate::MutexT,
    abstime: *const timespec,
) -> c_int {
    let deadline = Deadline {
        clock: cond_clock(cond),
        at: *abstime,
    };
    crate::cond_wait_impl(cond, lock, Some(deadline))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_clockwait(
    cond: *mut CondT,
    lock: *mut crate::MutexT,
    clock: clockid_t,
    abstime: *const timespec,
) -> c_int {
    if !supported_clock(clock) {
        return libc::EINVAL;
    }
    let deadline = Deadline {
        clock,
        at: *abstime,
    };
    crate::cond_wait_impl(cond, lock, Some(deadline))
}

pub(crate) fn valid_deadline(deadline: Option<Deadline>) -> bool {
    deadline.is_none_or(|d| valid(&d.at))
}

// ── Reader-writer locks ───────────────────────────────────────────────────

unsafe fn rwlock_lock(
    rwlock: *mut libc::pthread_rwlock_t,
    write: bool,
    try_only: bool,
    deadline: Option<Deadline>,
    op: &'static str,
    instr_addr: u64,
) -> c_int {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    sync_point(caller, op, rwlock.cast(), instr_addr);
    let key = rwlock as usize;
    let tid = st().info[&caller].tid;
    let result = loop {
        let lock = st().rwlocks.entry(key).or_default();
        if lock.writer == tid {
            break libc::EDEADLK;
        }
        if write && lock.writer == 0 && lock.readers.is_empty() {
            lock.writer = tid;
            break 0;
        }
        if !write && lock.writer == 0 {
            lock.readers.push(tid);
            break 0;
        }
        if try_only {
            break libc::EBUSY;
        }
        if deadline.is_some_and(|d| !valid(&d.at)) {
            break libc::EINVAL;
        }
        lock.waiters.push(caller);
        if st().block_on(caller, WaitOn::RwLock(key), deadline) {
            break libc::ETIMEDOUT;
        }
    };
    rsched_gunlock();
    result
}

macro_rules! rwlock_entry {
    ($name:ident, $op:literal, $write:expr, $try:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(rwlock: *mut libc::pthread_rwlock_t) -> c_int {
            rwlock_lock(rwlock, $write, $try, None, $op, return_address())
        }
    };
}

rwlock_entry!(rsched_pthread_rwlock_rdlock, "rwlock_rdlock", false, false);
rwlock_entry!(rsched_pthread_rwlock_wrlock, "rwlock_wrlock", true, false);
rwlock_entry!(
    rsched_pthread_rwlock_tryrdlock,
    "rwlock_tryrdlock",
    false,
    true
);
rwlock_entry!(
    rsched_pthread_rwlock_trywrlock,
    "rwlock_trywrlock",
    true,
    true
);

macro_rules! rwlock_timed_entry {
    ($name:ident, $clock_name:ident, $op:literal, $write:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            rwlock: *mut libc::pthread_rwlock_t,
            abstime: *const timespec,
        ) -> c_int {
            let deadline = Deadline {
                clock: libc::CLOCK_REALTIME,
                at: *abstime,
            };
            rwlock_lock(rwlock, $write, false, Some(deadline), $op, return_address())
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $clock_name(
            rwlock: *mut libc::pthread_rwlock_t,
            clock: clockid_t,
            abstime: *const timespec,
        ) -> c_int {
            if !supported_clock(clock) {
                return libc::EINVAL;
            }
            let deadline = Deadline {
                clock,
                at: *abstime,
            };
            rwlock_lock(rwlock, $write, false, Some(deadline), $op, return_address())
        }
    };
}

rwlock_timed_entry!(
    rsched_pthread_rwlock_timedrdlock,
    rsched_pthread_rwlock_clockrdlock,
    "rwlock_timedrdlock",
    false
);
rwlock_timed_entry!(
    rsched_pthread_rwlock_timedwrlock,
    rsched_pthread_rwlock_clockwrlock,
    "rwlock_timedwrlock",
    true
);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_rwlock_unlock(
    rwlock: *mut libc::pthread_rwlock_t,
) -> c_int {
    ensure_init();
    let instr_addr = return_address();
    rsched_glock();
    let caller = my_pt();
    let key = rwlock as usize;
    let tid = st().info[&caller].tid;
    let result = match st().rwlocks.get_mut(&key) {
        Some(lock) if lock.writer == tid => {
            lock.writer = 0;
            0
        }
        Some(lock) => match lock.readers.iter().position(|&r| r == tid) {
            Some(i) => {
                lock.readers.swap_remove(i);
                0
            }
            None => libc::EPERM,
        },
        None => libc::EPERM,
    };
    if result == 0 {
        let lock = st().rwlocks.get_mut(&key).unwrap();
        let waiters = std::mem::take(&mut lock.waiters);
        if lock.writer == 0 && lock.readers.is_empty() {
            st().rwlocks.remove(&key);
        }
        st().wake_all(waiters);
    }
    sync_point(caller, "rwlock_unlock", rwlock.cast(), instr_addr);
    rsched_gunlock();
    result
}

// ── pthread_once ──────────────────────────────────────────────────────────
//
// The once control's own state says whether initialization finished, so a
// control reused at the same address after being freed is never mistaken for
// a finished one. The model tracks only which controls are being initialized.
// An init routine that unwinds (C++ exception, cancellation) aborts the
// process instead of resetting the control.

/// glibc's __PTHREAD_ONCE_DONE bit, and musl's finished state.
const ONCE_DONE: i32 = 2;

unsafe fn once_done(control: *mut libc::pthread_once_t) -> bool {
    let value = AtomicI32::from_ptr(control.cast()).load(Ordering::Acquire);
    #[cfg(target_env = "musl")]
    {
        value == ONCE_DONE
    }
    #[cfg(not(target_env = "musl"))]
    {
        value & ONCE_DONE != 0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_once(
    control: *mut libc::pthread_once_t,
    init: unsafe extern "C" fn(),
) -> c_int {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    sync_point(caller, "pthread_once", control.cast(), return_address());
    let key = control as usize;
    loop {
        if once_done(control) {
            rsched_gunlock();
            return 0;
        }
        match st().once_running.get_mut(&key) {
            Some(waiters) => {
                waiters.push(caller);
                st().block_on(caller, WaitOn::Once, None);
            }
            None => {
                st().once_running.insert(key, Vec::new());
                break;
            }
        }
    }
    rsched_gunlock();
    // The init routine is application code: its own pthread calls and
    // atomics must reach rsched, so leave the wrapper's nesting depth first.
    #[cfg(feature = "instrumented-libc")]
    crate::depth_exit();
    init();
    #[cfg(feature = "instrumented-libc")]
    crate::depth_enter();
    rsched_glock();
    AtomicI32::from_ptr(control.cast()).store(ONCE_DONE, Ordering::Release);
    if let Some(waiters) = st().once_running.remove(&key) {
        st().wake_all(waiters);
    }
    rsched_gunlock();
    0
}

// ── Sleeps ────────────────────────────────────────────────────────────────

/// Sleeps the calling task until `deadline` (None: a zero-length sleep,
/// which is only a scheduling point). The deadline passes in virtual time.
/// Sleeps until `deadline`, as a scheduling point. Returns the nanoseconds
/// left when a signal handler interrupts the sleep, which then fails with
/// EINTR.
unsafe fn sleep_impl(deadline: Option<Deadline>, instr_addr: u64) -> Option<i64> {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    if !st().threads.contains(&caller) {
        // Not a task rsched schedules, such as a thread running TLS
        // destructors after rsched retired it: no other task can wake it,
        // so the sleep ends at once, with time moved to its deadline.
        rsched_gunlock();
        if let Some(deadline) = deadline {
            virt::advance_to(deadline.clock, &deadline.at);
        }
        return None;
    }
    sync_point(caller, "sleep", std::ptr::null(), instr_addr);
    let Some(deadline) = deadline else {
        rsched_gunlock();
        return None;
    };
    loop {
        st().block_on(caller, WaitOn::Sleep, Some(deadline));
        let interrupted = st().task(caller).take_interrupted();
        rsched_gunlock();
        if !interrupted {
            return None;
        }
        if crate::signals::deliver(false) {
            return Some(virt::remaining_ns(deadline.clock, &deadline.at));
        }
        if passed(deadline) {
            return None;
        }
        rsched_glock();
    }
}

/// The deadline `ms` milliseconds from now on the monotonic clock, for
/// I/O waits with a timeout.
pub(crate) unsafe fn deadline_in_ms(ms: c_int) -> Deadline {
    let ms = i64::from(ms);
    after(
        libc::CLOCK_MONOTONIC,
        timespec {
            tv_sec: ms / 1000,
            tv_nsec: (ms % 1000) * 1_000_000,
        },
    )
}

/// Whether virtual time has reached `deadline`.
pub(crate) unsafe fn passed(deadline: Deadline) -> bool {
    virt::remaining_ns(deadline.clock, &deadline.at) == 0
}

/// Blocks `caller`, already marked as waiting on I/O, until a descriptor
/// wakes it or `deadline` passes; returns whether it passed. The deadline
/// expires like any timed wait: once virtual time reaches it, or when no
/// task can run.
pub(crate) unsafe fn block_until_deadline(caller: PthreadT, deadline: Deadline) -> bool {
    st().block_on(caller, WaitOn::Sleep, Some(deadline))
}

/// Sleeps `ns` nanoseconds of virtual time, as a scheduling point; zero
/// only yields. For kernel waits that watch no descriptor, such as
/// `select(0, NULL, NULL, NULL, &tv)`, which programs use as a sleep.
/// Returns whether a signal handler interrupted the sleep.
pub(crate) unsafe fn sleep_for(ns: i64, instr_addr: u64) -> bool {
    let rel = timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: ns % 1_000_000_000,
    };
    let deadline = (ns > 0).then(|| after(libc::CLOCK_MONOTONIC, rel));
    sleep_impl(deadline, instr_addr).is_some()
}

/// Splits nanoseconds into a timespec.
fn to_timespec(ns: i64) -> timespec {
    timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: ns % 1_000_000_000,
    }
}

fn is_zero(ts: &timespec) -> bool {
    ts.tv_sec == 0 && ts.tv_nsec == 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_nanosleep(req: *const timespec, rem: *mut timespec) -> c_int {
    if req.is_null() {
        set_errno(libc::EFAULT);
        return -1;
    }
    let req = *req;
    if !valid(&req) || req.tv_sec < 0 {
        set_errno(libc::EINVAL);
        return -1;
    }
    let deadline = (!is_zero(&req)).then(|| after(libc::CLOCK_MONOTONIC, req));
    if let Some(left) = sleep_impl(deadline, return_address()) {
        if !rem.is_null() {
            *rem = to_timespec(left);
        }
        set_errno(libc::EINTR);
        return -1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_clock_nanosleep(
    clock: clockid_t,
    flags: c_int,
    req: *const timespec,
    rem: *mut timespec,
) -> c_int {
    if req.is_null() {
        return libc::EFAULT;
    }
    let req = *req;
    if !valid(&req) || req.tv_sec < 0 {
        return libc::EINVAL;
    }
    if !virt::is_virtual(clock) {
        // CPU-time and other clocks: leave the sleep to the kernel.
        let r = seccomp::raw_syscall6(
            libc::SYS_clock_nanosleep,
            clock as c_long,
            flags as c_long,
            &req as *const timespec as c_long,
            rem as c_long,
            0,
            0,
        );
        return (-r) as c_int;
    }
    let deadline = if flags & libc::TIMER_ABSTIME != 0 {
        Some(Deadline { clock, at: req })
    } else {
        (!is_zero(&req)).then(|| after(clock, req))
    };
    if let Some(left) = sleep_impl(deadline, return_address()) {
        if flags & libc::TIMER_ABSTIME == 0 && !rem.is_null() {
            *rem = to_timespec(left);
        }
        return libc::EINTR;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_usleep(usec: libc::useconds_t) -> c_int {
    let req = timespec {
        tv_sec: (usec / 1_000_000) as libc::time_t,
        tv_nsec: (usec % 1_000_000) as libc::c_long * 1000,
    };
    let deadline = (!is_zero(&req)).then(|| after(libc::CLOCK_MONOTONIC, req));
    if sleep_impl(deadline, return_address()).is_some() {
        set_errno(libc::EINTR);
        return -1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sleep(seconds: c_uint) -> c_uint {
    let req = timespec {
        tv_sec: seconds as libc::time_t,
        tv_nsec: 0,
    };
    let deadline = (seconds != 0).then(|| after(libc::CLOCK_MONOTONIC, req));
    match sleep_impl(deadline, return_address()) {
        Some(left) => ((left + 999_999_999) / 1_000_000_000) as c_uint,
        None => 0,
    }
}
