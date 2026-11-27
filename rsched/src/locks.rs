//! Kernel file locks and SysV semaphores as scheduling points.
//!
//! `fcntl` record locks, `flock`, and `semop` can block in the kernel until
//! another process releases the lock, which under rsched happens only once
//! the blocked task gives up the turn. Each lock operation is therefore a
//! scheduling point that then runs in its non-blocking form. When that
//! reports it would block, the task waits, not runnable, like a task
//! waiting for a mutex, until some task releases a kernel lock: an unlock,
//! a semaphore change, a close, or a process exit. rsched cannot tell which
//! lock a release frees, so each release makes every lock waiter runnable,
//! and a waiter that still cannot take its lock waits again.
//!
//! Once no task can run and some wait for locks, rsched sleeps briefly in
//! real time and lets them try once more, in case a lock was released in a
//! way it does not watch, such as a descriptor closed on exec. If none of
//! them gets its lock, the tasks are deadlocked and rsched reports it (see
//! `task_provider::retry_lock_waiters`).

use crate::seccomp::raw_syscall6;
use crate::sync::{Deadline, WaitOn};
use libc::{c_int, c_long};

// On x86_64, F_SETLK64 and F_SETLKW64 are F_SETLK and F_SETLKW.
const F_SETLK: c_long = libc::F_SETLK as c_long;
const F_SETLKW: c_long = libc::F_SETLKW as c_long;
const F_OFD_SETLK: c_long = 37;
const F_OFD_SETLKW: c_long = 38;
/// More operations than any kernel accepts in one semop (SEMOPM is 500).
const MAX_SEMOPS: c_long = 4096;

/// Syscalls that take, release, or remove kernel locks; the seccomp filter
/// traps these.
pub(crate) const LOCK_SYSCALLS: &[c_long] = &[
    libc::SYS_fcntl,
    libc::SYS_flock,
    libc::SYS_semop,
    libc::SYS_semtimedop,
    libc::SYS_semctl,
];

pub(crate) fn is_lock_syscall(nr: c_long) -> bool {
    LOCK_SYSCALLS.contains(&nr)
}

fn is_setlk(cmd: c_long) -> bool {
    matches!(cmd, F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW)
}

fn is_semctl_release(cmd: c_long) -> bool {
    // glibc adds IPC_64 to the command.
    matches!(
        (cmd & !0x100) as c_int,
        libc::IPC_RMID | libc::SETVAL | libc::SETALL
    )
}

/// Whether the call is a lock operation and so a scheduling point; other
/// `fcntl` and `semctl` commands run unchanged.
fn is_lock_op(nr: c_long, args: &[c_long; 6]) -> bool {
    match nr {
        libc::SYS_fcntl => is_setlk(args[1]),
        libc::SYS_semctl => is_semctl_release(args[2]),
        _ => true,
    }
}

/// The semaphore operations of a semop call, or None for arguments the
/// kernel rejects without looking at them.
unsafe fn semops<'a>(args: &[c_long; 6]) -> Option<&'a [libc::sembuf]> {
    let (sops, nsops) = (args[1] as *const libc::sembuf, args[2]);
    (!sops.is_null() && (1..=MAX_SEMOPS).contains(&nsops))
        .then(|| core::slice::from_raw_parts(sops, nsops as usize))
}

/// Whether the call, if it succeeds, can let a lock waiter proceed.
unsafe fn releases(nr: c_long, args: &[c_long; 6]) -> bool {
    match nr {
        libc::SYS_fcntl => {
            let fl = args[2] as *const libc::flock;
            // Unlocking, or turning a write lock into a read lock.
            !fl.is_null() && matches!(c_int::from((*fl).l_type), libc::F_UNLCK | libc::F_RDLCK)
        }
        libc::SYS_flock => args[1] & c_long::from(libc::LOCK_UN | libc::LOCK_SH) != 0,
        // A decrement can bring a semaphore to the zero others wait for.
        libc::SYS_semop | libc::SYS_semtimedop => {
            semops(args).is_some_and(|ops| ops.iter().any(|op| op.sem_op != 0))
        }
        libc::SYS_semctl => true,
        _ => false,
    }
}

/// A call that may block, made in a form that cannot.
struct Attempt {
    args: [c_long; 6],
    /// A semop's operations with IPC_NOWAIT set, which `args` points to.
    _sops: Vec<libc::sembuf>,
    deadline: Option<Deadline>,
}

/// The non-blocking form of a call that may block, or None when the call
/// cannot block: a release, an explicit try, or arguments the kernel
/// rejects at once.
unsafe fn attempt(nr: c_long, args: &[c_long; 6]) -> Option<Attempt> {
    let mut try_args = *args;
    let mut sops = Vec::new();
    let mut deadline = None;
    match nr {
        libc::SYS_fcntl => {
            let fl = args[2] as *const libc::flock;
            let waits = matches!(args[1], F_SETLKW | F_OFD_SETLKW);
            if !waits || fl.is_null() || c_int::from((*fl).l_type) == libc::F_UNLCK {
                return None;
            }
            try_args[1] = if args[1] == F_OFD_SETLKW {
                F_OFD_SETLK
            } else {
                F_SETLK
            };
        }
        libc::SYS_flock => {
            let op = args[1];
            if op & c_long::from(libc::LOCK_NB | libc::LOCK_UN) != 0 {
                return None;
            }
            try_args[1] = op | c_long::from(libc::LOCK_NB);
        }
        libc::SYS_semop | libc::SYS_semtimedop => {
            let ops = semops(args)?;
            let nowait = libc::IPC_NOWAIT as libc::c_short;
            // Only decrements and waits for zero block. A call that mixes
            // them with IPC_NOWAIT operations is treated as blocking
            // throughout: rsched cannot tell which operation the kernel
            // found unable to proceed.
            if ops
                .iter()
                .all(|op| op.sem_op > 0 || op.sem_flg & nowait != 0)
            {
                return None;
            }
            sops = ops.to_vec();
            for op in &mut sops {
                op.sem_flg |= nowait;
            }
            try_args[1] = sops.as_ptr() as c_long;
            if nr == libc::SYS_semtimedop {
                let timeout = args[3] as *const libc::timespec;
                if !timeout.is_null() {
                    let ts = *timeout;
                    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
                        return None;
                    }
                    deadline = Some(crate::sync::after(libc::CLOCK_MONOTONIC, ts));
                    try_args[3] = 0;
                }
            }
        }
        _ => return None,
    }
    Some(Attempt {
        args: try_args,
        _sops: sops,
        deadline,
    })
}

fn would_block(nr: c_long, r: c_long) -> bool {
    r == -c_long::from(libc::EAGAIN) || (nr == libc::SYS_fcntl && r == -c_long::from(libc::EACCES))
}

unsafe fn raw(nr: c_long, args: &[c_long; 6]) -> c_long {
    raw_syscall6(nr, args[0], args[1], args[2], args[3], args[4], args[5])
}

/// Blocks the calling task until a lock release wakes it, `deadline`
/// passes, or a signal interrupts it. Returns whether the deadline passed.
unsafe fn wait_for_release(deadline: Option<Deadline>) -> bool {
    crate::rsched_glock();
    let caller = crate::my_pt();
    let task = crate::st().task(caller);
    task.set_lock_wait(true);
    // A sleep, as far as signals and timeouts are concerned.
    let timed_out = crate::st().block_on(caller, WaitOn::Sleep, deadline);
    task.set_lock_wait(false);
    crate::rsched_gunlock();
    timed_out
}

/// After syscall `nr` returned `r`: a call that closed a descriptor drops
/// the process's record locks on its file, and possibly the file's last
/// flock lock, so it wakes the lock waiters.
pub(crate) unsafe fn after_close(nr: c_long, r: c_long) {
    if r >= 0 && crate::io::FD_GUARDED_SYSCALLS.contains(&nr) {
        released();
    }
}

/// Makes every task waiting for a kernel lock runnable.
pub(crate) unsafe fn released() {
    if crate::task_provider::lock_waiters_exist() {
        crate::rsched_glock();
        crate::task_provider::wake_lock_waiters();
        crate::rsched_gunlock();
    }
}

/// Runs lock syscall `nr`, a scheduling point when `schedule` is set,
/// without blocking in the kernel, and returns the raw result or -errno.
pub(crate) unsafe fn syscall(
    instr_addr: u64,
    nr: c_long,
    args: &[c_long; 6],
    schedule: bool,
) -> c_long {
    if !is_lock_op(nr, args) {
        return raw(nr, args);
    }
    crate::ensure_init();
    if schedule {
        crate::io::io_point(instr_addr, args[0] as c_int, nr);
    }
    let release = releases(nr, args);
    let r = match attempt(nr, args) {
        None => raw(nr, args),
        Some(attempt) => {
            let mut first = true;
            loop {
                let r = raw(nr, &attempt.args);
                // Taking a lock, or starting to wait for one, is progress; a
                // woken waiter that waits again is not.
                if r >= 0 || first {
                    crate::task_provider::lock_progress();
                }
                first = false;
                if !would_block(nr, r) {
                    break r;
                }
                if wait_for_release(attempt.deadline) {
                    break -c_long::from(libc::EAGAIN);
                }
                // SA_RESTART restarts file locks but never semop.
                if crate::signals::interrupts(nr != libc::SYS_semop && nr != libc::SYS_semtimedop) {
                    break -c_long::from(libc::EINTR);
                }
            }
        }
    };
    if r >= 0 && release {
        released();
    }
    r
}

unsafe fn libc_call(nr: c_long, args: [c_long; 6]) -> c_long {
    crate::io::libc_result(syscall(crate::return_address(), nr, &args, true))
}

/// `fcntl`, whose third argument is an integer or a pointer. Commands other
/// than record locks go to libc's own fcntl, which adjusts some results.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fcntl(fd: c_int, cmd: c_int, arg: c_long) -> c_int {
    let args = [fd.into(), cmd.into(), arg, 0, 0, 0];
    if !is_lock_op(libc::SYS_fcntl, &args) {
        return libc::fcntl(fd, cmd, arg);
    }
    libc_call(libc::SYS_fcntl, args) as c_int
}

/// `lockf`, as the record lock on `len` bytes from the file offset that
/// glibc's lockf takes with fcntl. glibc's lockf calls its fcntl through a
/// hidden alias that the fcntl wrapper does not see, so it is wrapped too.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_lockf(fd: c_int, cmd: c_int, len: libc::off_t) -> c_int {
    let (lock_cmd, l_type) = match cmd {
        libc::F_LOCK => (F_SETLKW, libc::F_WRLCK),
        libc::F_TLOCK => (F_SETLK, libc::F_WRLCK),
        libc::F_ULOCK => (F_SETLK, libc::F_UNLCK),
        // F_TEST only reads the lock, and other commands fail.
        _ => return libc::lockf(fd, cmd, len),
    };
    let mut fl: libc::flock = core::mem::zeroed();
    fl.l_type = l_type as libc::c_short;
    fl.l_whence = libc::SEEK_CUR as libc::c_short;
    fl.l_len = len;
    let args = [
        fd.into(),
        lock_cmd,
        &mut fl as *mut libc::flock as c_long,
        0,
        0,
        0,
    ];
    libc_call(libc::SYS_fcntl, args) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_flock(fd: c_int, op: c_int) -> c_int {
    libc_call(libc::SYS_flock, [fd.into(), op.into(), 0, 0, 0, 0]) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_semop(id: c_int, sops: *mut libc::sembuf, nsops: usize) -> c_int {
    let args = [id.into(), sops as c_long, nsops as c_long, 0, 0, 0];
    libc_call(libc::SYS_semop, args) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_semtimedop(
    id: c_int,
    sops: *mut libc::sembuf,
    nsops: usize,
    timeout: *const libc::timespec,
) -> c_int {
    let args = [
        id.into(),
        sops as c_long,
        nsops as c_long,
        timeout as c_long,
        0,
        0,
    ];
    libc_call(libc::SYS_semtimedop, args) as c_int
}

/// `semctl`, whose fourth argument is a `union semun` passed by value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_semctl(id: c_int, num: c_int, cmd: c_int, arg: c_long) -> c_int {
    let args = [id.into(), num.into(), cmd.into(), arg, 0, 0];
    if !is_lock_op(libc::SYS_semctl, &args) {
        return libc::semctl(id, num, cmd, arg);
    }
    libc_call(libc::SYS_semctl, args) as c_int
}
