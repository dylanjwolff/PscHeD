//! Signals between scheduled processes.
//!
//! A signal that one process sends another must reach it at a point the
//! schedule decides, not whenever the kernel delivers it: a handler running
//! in a parked thread would race the task that holds the turn, and a
//! process killed while parked would never hand the schedule on.
//!
//! So a thread blocks every asynchronous signal whenever rsched parks it in
//! the kernel ([`hold`]), and restores the program's mask when it returns to
//! the program from its scheduling point ([`deliver`]); signals sent
//! meanwhile stay pending and run there, while the thread holds the turn.
//! The sender ([`rsched_kill`]) makes the target's tasks that are blocked in
//! an interruptible wait, I/O or a sleep, runnable, and the woken task fails
//! its call with EINTR once a handler ran, or waits again when every
//! handler that ran has SA_RESTART and the call restarts. A signal whose
//! default action ends the process first hands the schedule on, then ends
//! it. SIGKILL cannot be held: the sender waits for the target to die and
//! retires it.
//!
//! `pthread_kill` works the same way for one thread of the caller's own
//! process ([`rsched_pthread_kill`]).
//!
//! Not modeled: a signal does not interrupt mutex, semaphore, or condition
//! variable waits, so a process blocked only in those handles it once it
//! gets the lock; stop signals do not stop a process; and signals the
//! kernel generates, such as SIGCHLD, arrive whenever it sends them.

use crate::seccomp::raw_syscall6;
use libc::{c_int, c_long};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// Signal `sig`'s bit in a kernel signal set.
const fn bit(sig: c_int) -> u64 {
    1 << (sig - 1)
}

/// The signals [`hold`] blocks: all but those the kernel raises for a
/// fault in the thread itself, SIGSYS for the seccomp tripwire, SIGABRT for
/// rsched's own panics, and the two real-time signals glibc uses
/// internally, for thread cancellation and setxid.
const HELD_SET: u64 = !(bit(libc::SIGKILL)
    | bit(libc::SIGSTOP)
    | bit(libc::SIGSEGV)
    | bit(libc::SIGBUS)
    | bit(libc::SIGFPE)
    | bit(libc::SIGILL)
    | bit(libc::SIGTRAP)
    | bit(libc::SIGSYS)
    | bit(libc::SIGABRT)
    | bit(32)
    | bit(33));

/// The program's signal mask of each thread that [`hold`] blocked signals
/// in, keyed by thread id. A thread's own entry is only touched by that
/// thread. Keyed by thread id rather than thread-local storage because
/// coroutine tasks share one kernel thread, and with it one signal mask.
const HELD_ENTRIES: usize = 4096;
static HELD_TIDS: [AtomicI32; HELD_ENTRIES] = [const { AtomicI32::new(0) }; HELD_ENTRIES];
static HELD: [AtomicBool; HELD_ENTRIES] = [const { AtomicBool::new(false) }; HELD_ENTRIES];
static HELD_MASKS: [AtomicU64; HELD_ENTRIES] = [const { AtomicU64::new(0) }; HELD_ENTRIES];

fn gettid() -> c_int {
    unsafe { raw_syscall6(libc::SYS_gettid, 0, 0, 0, 0, 0, 0) as c_int }
}

/// The calling thread's entry, claiming a free one when `claim` is set.
fn entry(claim: bool) -> Option<usize> {
    let tid = gettid();
    let start = tid as usize % HELD_ENTRIES;
    for probe in 0..HELD_ENTRIES {
        let idx = (start + probe) % HELD_ENTRIES;
        let seen = HELD_TIDS[idx].load(Ordering::Acquire);
        if seen == tid {
            return Some(idx);
        }
        if seen == 0 {
            if !claim {
                return None;
            }
            if HELD_TIDS[idx]
                .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(idx);
            }
        }
    }
    None
}

unsafe fn sigprocmask(how: c_int, set: *const u64, old: *mut u64) {
    raw_syscall6(
        libc::SYS_rt_sigprocmask,
        how.into(),
        set as c_long,
        old as c_long,
        8,
        0,
        0,
    );
}

/// Blocks asynchronous signals in the calling thread until it next returns
/// to the program, because rsched is about to park it. Does nothing when
/// they are already held.
pub(crate) unsafe fn hold() {
    let Some(idx) = entry(true) else {
        return;
    };
    if HELD[idx].load(Ordering::Relaxed) {
        return;
    }
    let mut old = 0u64;
    sigprocmask(libc::SIG_BLOCK, &HELD_SET, &mut old);
    HELD_MASKS[idx].store(old, Ordering::Relaxed);
    HELD[idx].store(true, Ordering::Relaxed);
}

/// The program's mask, if [`hold`] blocked signals in this thread, for a
/// forked child to [`inherit`].
pub(crate) fn held_mask() -> Option<u64> {
    let idx = entry(false)?;
    HELD[idx]
        .load(Ordering::Relaxed)
        .then(|| HELD_MASKS[idx].load(Ordering::Relaxed))
}

/// In a forked child: forgets the parent's threads and takes over the
/// forking thread's held state, since the child's only thread inherits its
/// signal mask but has a new thread id.
pub(crate) fn inherit(held: Option<u64>) {
    for idx in 0..HELD_ENTRIES {
        HELD_TIDS[idx].store(0, Ordering::Relaxed);
        HELD[idx].store(false, Ordering::Relaxed);
    }
    if let (Some(mask), Some(idx)) = (held, entry(true)) {
        HELD_MASKS[idx].store(mask, Ordering::Relaxed);
        HELD[idx].store(true, Ordering::Relaxed);
    }
}

/// In a new thread: takes over its creator's held state. The thread starts
/// with its creator's signal mask, which is rsched's blocked mask when the
/// creator's signals were held; its program mask is the creator's. Without
/// this, the thread's first [`hold`] would save the blocked mask as the
/// program's, and no signal would ever run in it. Also forgets a held state
/// left by an exited thread with the same id.
pub(crate) fn adopt(held: Option<u64>) {
    let Some(idx) = entry(held.is_some()) else {
        return;
    };
    if let Some(mask) = held {
        HELD_MASKS[idx].store(mask, Ordering::Relaxed);
    }
    HELD[idx].store(held.is_some(), Ordering::Relaxed);
}

/// The program's mask, if [`hold`] blocked signals in this thread; it no
/// longer counts as held.
fn take_held() -> Option<u64> {
    let idx = entry(false)?;
    HELD[idx]
        .swap(false, Ordering::Relaxed)
        .then(|| HELD_MASKS[idx].load(Ordering::Relaxed))
}

/// Whether a signal that the program's mask lets through is pending while
/// rsched holds signals in this thread, so an interruptible wait must not
/// block.
/// Whether `task`, the caller, must not block in an interruptible wait
/// because a signal is pending for it (see [`pending`]) or, under coro,
/// was sent to it while it was not blocked (see [`rsched_pthread_kill`]).
pub(crate) unsafe fn pending_for(task: crate::task_provider::TaskHandle) -> bool {
    pending() || (cfg!(feature = "coro") && task.is_interrupted())
}

pub(crate) unsafe fn pending() -> bool {
    let Some(idx) = entry(false) else {
        return false;
    };
    if !HELD[idx].load(Ordering::Relaxed) {
        return false;
    }
    pending_set() & !HELD_MASKS[idx].load(Ordering::Relaxed) & HELD_SET != 0
}

unsafe fn pending_set() -> u64 {
    let mut pending = 0u64;
    raw_syscall6(
        libc::SYS_rt_sigpending,
        &mut pending as *mut u64 as c_long,
        8,
        0,
        0,
        0,
        0,
    );
    pending
}

/// Whether `sig`'s default action ends the process. The others are ignored
/// or stop it, which rsched does not model.
fn ends_process(sig: c_int) -> bool {
    !matches!(
        sig,
        libc::SIGCHLD
            | libc::SIGCONT
            | libc::SIGURG
            | libc::SIGWINCH
            | libc::SIGSTOP
            | libc::SIGTSTP
            | libc::SIGTTIN
            | libc::SIGTTOU
    )
}

/// Restores the program's signal mask if rsched held signals in this
/// thread, so the signals sent meanwhile run now, at a point where the
/// thread holds the turn. Must be called without GMTX. Returns whether a
/// handler ran and the interrupted call must fail with EINTR: when it is
/// not `restartable`, or a handler that ran lacks SA_RESTART.
pub(crate) unsafe fn deliver(restartable: bool) -> bool {
    let Some(mask) = take_held() else {
        return false;
    };
    let pending = pending_set();
    let mut handled = false;
    let mut restarts = true;
    let mut fatal = None;
    for sig in 1..=64 {
        if pending & !mask & HELD_SET & bit(sig) == 0 {
            continue;
        }
        // The kernel's sigaction: handler, flags, restorer, mask.
        let mut action = [0usize; 4];
        raw_syscall6(
            libc::SYS_rt_sigaction,
            sig.into(),
            0,
            action.as_mut_ptr() as c_long,
            8,
            0,
            0,
        );
        match action[0] {
            h if h == libc::SIG_IGN => {}
            h if h == libc::SIG_DFL => {
                if ends_process(sig) {
                    fatal = Some(sig);
                }
            }
            _ => {
                handled = true;
                restarts &= action[1] & libc::SA_RESTART as usize != 0;
            }
        }
    }
    if let Some(sig) = fatal {
        // The signal ends this process as soon as it is unblocked, so hand
        // the schedule on first, as exit does.
        crate::rsched_process_exit();
        sigprocmask(libc::SIG_SETMASK, &mask, core::ptr::null_mut());
        let pid = raw_syscall6(libc::SYS_getpid, 0, 0, 0, 0, 0, 0);
        raw_syscall6(libc::SYS_kill, pid, sig.into(), 0, 0, 0, 0);
        raw_syscall6(libc::SYS_exit_group, 128 + c_long::from(sig), 0, 0, 0, 0, 0);
    }
    // Handlers run here, as if called from the program, so that the
    // functions they call reach rsched's models.
    crate::with_outer_depth(|| sigprocmask(libc::SIG_SETMASK, &mask, core::ptr::null_mut()));
    if cfg!(feature = "coro") && handled {
        let restart_bit = if restarts { CORO_HANDLED_RESTARTS } else { 0 };
        CORO_HANDLED.store(CORO_HANDLED_RAN | restart_bit, Ordering::Relaxed);
    }
    handled && !(restartable && restarts)
}

/// After a blocking wait returns: when a signal woke the calling task, runs
/// its handler and returns whether the call must fail with EINTR (see
/// [`deliver`]). Must be called without GMTX.
pub(crate) unsafe fn interrupts(restartable: bool) -> bool {
    crate::rsched_glock();
    let caller = crate::my_pt();
    let interrupted = crate::st().threads.contains(&caller) && crate::st().task(caller).take_interrupted();
    crate::rsched_gunlock();
    if !interrupted {
        return false;
    }
    let ran_here = deliver(restartable);
    // Under coro, the handler may have run in another task (see
    // CORO_HANDLED).
    let record = CORO_HANDLED.swap(0, Ordering::Relaxed);
    ran_here
        || (record & CORO_HANDLED_RAN != 0 && !(restartable && record & CORO_HANDLED_RESTARTS != 0))
}

/// Waits in the kernel until process `pid` has died.
pub(crate) unsafe fn wait_dead(pid: libc::pid_t) {
    const SYS_PIDFD_OPEN: c_long = 434;
    let fd = raw_syscall6(SYS_PIDFD_OPEN, pid.into(), 0, 0, 0, 0, 0);
    if fd < 0 {
        return;
    }
    let mut pfd = libc::pollfd {
        fd: fd as c_int,
        events: libc::POLLIN,
        revents: 0,
    };
    while raw_syscall6(
        libc::SYS_poll,
        &mut pfd as *mut libc::pollfd as c_long,
        1,
        -1,
        0,
        0,
        0,
    ) < 0
    {}
    raw_syscall6(libc::SYS_close, fd, 0, 0, 0, 0, 0);
}

/// Under coro, every task of a process shares one kernel thread, so a
/// signal sent to one task's thread may run its handler in whichever task
/// returns to the program first. That task records what ran here, and the
/// interrupted task takes the record if no handler ran in it.
static CORO_HANDLED: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
const CORO_HANDLED_RAN: u8 = 1;

/// Forgets handlers that ran before a task was interrupted, so they do not
/// make its wait fail.
pub(crate) fn forget_coro_handled() {
    CORO_HANDLED.store(0, Ordering::Relaxed);
}
const CORO_HANDLED_RESTARTS: u8 = 2;

/// `pthread_kill`, as a scheduling point: sends `sig` to `thread` and, when
/// the thread is a task blocked in an interruptible wait, makes it runnable
/// so it handles the signal there, as for [`rsched_kill`]. Under coro the
/// signal goes to the process's one kernel thread, since a task's tid is
/// not a kernel thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_kill(thread: libc::pthread_t, sig: c_int) -> c_int {
    crate::ensure_init();
    hold();
    crate::rsched_glock();
    crate::sync::kill_point(crate::return_address());
    let target = thread as crate::PthreadT;
    let scheduled = crate::st().threads.contains(&target);
    let r = if cfg!(feature = "coro") {
        // Every thread is a task, and its pthread_t a task key that glibc's
        // pthread_kill cannot use. One that is no longer scheduled has
        // exited. Signals 32 and 33 are glibc's own, as glibc refuses too.
        if !scheduled {
            libc::ESRCH
        } else if !(0..=64).contains(&sig) || sig == 32 || sig == 33 {
            libc::EINVAL
        } else {
            // Only handlers that run from now on count for the target.
            CORO_HANDLED.store(0, Ordering::Relaxed);
            let pid = raw_syscall6(libc::SYS_getpid, 0, 0, 0, 0, 0, 0);
            let tid = raw_syscall6(libc::SYS_gettid, 0, 0, 0, 0, 0, 0);
            -(raw_syscall6(libc::SYS_tgkill, pid, tid, sig.into(), 0, 0, 0) as c_int)
        }
    } else {
        crate::with_internal_depth(|| libc::pthread_kill(thread, sig))
    };
    if r == 0 && sig != 0 && scheduled {
        let task = crate::st().task(target);
        // Under coro the handler may run in another task before the target
        // blocks, so nothing is pending when it does: mark it interrupted
        // now, and its next interruptible wait fails at once. This matches
        // native, where the signal stays pending in the target until a
        // wait, since rsched delivers held signals only on return from
        // calls that can be interrupted (see the README).
        if !task.interrupt() && cfg!(feature = "coro") {
            task.set_interrupted();
        }
    }
    crate::rsched_gunlock();
    // Back to the program: a signal the thread sent itself runs now.
    deliver(true);
    r
}

/// `kill`, as a scheduling point: sends `sig` and wakes the scheduled
/// processes it reaches so they can handle it (see the module docs).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_kill(pid: libc::pid_t, sig: c_int) -> c_int {
    crate::ensure_init();
    // A signal this process sends itself then runs, like one from another
    // process, when the call returns to the program.
    hold();
    crate::rsched_glock();
    crate::sync::kill_point(crate::return_address());
    let r = raw_syscall6(libc::SYS_kill, pid.into(), sig.into(), 0, 0, 0, 0);
    if r == 0 && sig != 0 {
        for (slot, target) in crate::task_provider::kill_targets(pid) {
            if sig == libc::SIGKILL {
                wait_dead(target);
                crate::task_provider::retire_process(slot);
            } else {
                crate::task_provider::interrupt_process(slot);
            }
        }
    }
    crate::rsched_gunlock();
    let r = crate::io::libc_result(r) as c_int;
    // Back to the program: a signal this process sent itself runs now.
    let errno = *libc::__errno_location();
    deliver(true);
    *libc::__errno_location() = errno;
    r
}
