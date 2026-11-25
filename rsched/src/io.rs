//! File and socket operations as scheduling points.
//!
//! Only one task runs at a time, so an I/O call that blocks in the kernel
//! would stall every other task, including the one that would unblock it.
//! Each modeled operation is therefore a scheduling point. When it would
//! block, the task stops being runnable until its descriptor is ready, like a
//! task waiting for a mutex, and the operation then runs as a raw syscall
//! that cannot block.
//!
//! Every integration mode funnels through [`prepare`]: the `rsched_*`
//! wrappers used by source annotations, the LLVM pass, and instrumented libc;
//! libc's `syscall()` trampoline; the e9patch hook for raw `syscall`
//! instructions; and the seccomp handler for I/O issued outside rsched.

use crate::event::{Event, EventKind};
use crate::seccomp::raw_syscall6;
use crate::sync::Deadline;
use crate::sys::{Epoll, Interest};
use libc::{c_int, c_long, c_void};

/// How a modeled syscall can block.
enum Blocking {
    /// Never blocks (or only briefly, such as `connect` on a local socket).
    Never,
    /// Blocks until the descriptor in the first argument reports `events`.
    Until(libc::c_short),
    /// `poll(fds, nfds, timeout)`.
    Poll,
}

fn classify(nr: c_long, args: &[c_long; 6]) -> Option<Blocking> {
    let dontwait = |flags: c_long| {
        if flags & c_long::from(libc::MSG_DONTWAIT) != 0 {
            Blocking::Never
        } else {
            Blocking::Until(libc::POLLIN)
        }
    };
    Some(match nr {
        libc::SYS_read
        | libc::SYS_readv
        | libc::SYS_pread64
        | libc::SYS_preadv
        | libc::SYS_accept
        | libc::SYS_accept4 => Blocking::Until(libc::POLLIN),
        libc::SYS_write | libc::SYS_writev | libc::SYS_pwrite64 | libc::SYS_pwritev => {
            Blocking::Until(libc::POLLOUT)
        }
        libc::SYS_recvfrom => dontwait(args[3]),
        libc::SYS_recvmsg => dontwait(args[2]),
        libc::SYS_sendto | libc::SYS_sendmsg => {
            let flags = if nr == libc::SYS_sendto {
                args[3]
            } else {
                args[2]
            };
            match dontwait(flags) {
                Blocking::Until(_) => Blocking::Until(libc::POLLOUT),
                never => never,
            }
        }
        libc::SYS_connect => Blocking::Never,
        libc::SYS_poll => Blocking::Poll,
        _ => return None,
    })
}

/// Syscall numbers the model covers; the seccomp filter traps these.
pub(crate) const MODELED_SYSCALLS: &[c_long] = &[
    libc::SYS_read,
    libc::SYS_readv,
    libc::SYS_pread64,
    libc::SYS_preadv,
    libc::SYS_write,
    libc::SYS_writev,
    libc::SYS_pwrite64,
    libc::SYS_pwritev,
    libc::SYS_recvfrom,
    libc::SYS_recvmsg,
    libc::SYS_sendto,
    libc::SYS_sendmsg,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_connect,
    libc::SYS_poll,
];

pub(crate) fn is_modeled(nr: c_long) -> bool {
    MODELED_SYSCALLS.contains(&nr)
}

// The syscalls here that the seccomp filter traps (poll, and the
// descriptor-closing calls in `guarded_syscall`) must go through rsched's own
// syscall instruction, `raw_syscall6`, which the filter lets through. Others
// use the safe wrappers in `sys`, which make them the same way.

unsafe fn poll_one(fd: c_int, events: libc::c_short) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // Ready, hung up, or an error the real call will report.
    raw_syscall6(libc::SYS_poll, &mut pfd as *mut _ as c_long, 1, 0, 0, 0, 0) != 0
}

fn is_nonblocking(fd: c_int) -> bool {
    // A bad descriptor, such as -1, fails immediately in the real call.
    if fd < 0 {
        return true;
    }
    crate::sys::status_flags(fd).map_or(true, |flags| flags & libc::O_NONBLOCK != 0)
}

unsafe fn io_point(instr_addr: u64, fd: c_int, syscall: c_long) {
    crate::rsched_glock();
    let caller = crate::my_pt();
    crate::st().set_event(
        caller,
        Event {
            instr_addr,
            kind: EventKind::Io { fd, syscall },
        },
    );
    crate::st().context_switch(caller);
    crate::st().clear_event(caller);
    crate::rsched_gunlock();
}

/// Blocks the calling task until child process `pid` changes state, by
/// watching its pidfd like any other descriptor. Returns false, without
/// blocking, for a `pid` that names no single process or when the kernel
/// has no pidfds; the caller then yields instead.
pub(crate) unsafe fn wait_for_child(pid: libc::pid_t) -> bool {
    const SYS_PIDFD_OPEN: c_long = 434;
    if pid <= 0 {
        return false;
    }
    let fd = raw_syscall6(SYS_PIDFD_OPEN, pid as c_long, 0, 0, 0, 0, 0);
    if fd < 0 {
        return false;
    }
    wait_until_ready(&[libc::pollfd {
        fd: fd as c_int,
        events: libc::POLLIN,
        revents: 0,
    }]);
    raw_syscall6(libc::SYS_close, fd, 0, 0, 0, 0, 0);
    true
}

/// Blocks the calling task, which is not runnable meanwhile, until one of
/// `fds` reports its events, an error, or a hang-up. Ready descriptors can
/// become unready again before the task runs, so callers check again.
unsafe fn wait_until_ready(fds: &[libc::pollfd]) {
    wait_until_ready_or(fds, None);
}

/// Like [`wait_until_ready`], but also returns, reporting true, once
/// `deadline` passes in virtual time.
unsafe fn wait_until_ready_or(fds: &[libc::pollfd], deadline: Option<Deadline>) -> bool {
    match block_until_ready(fds, deadline) {
        Some(timed_out) => timed_out,
        None => {
            // rsched cannot watch these descriptors, for example a user
            // epoll nested as deep as the kernel allows: let the other tasks
            // run once before the caller checks again.
            crate::rsched_sched_yield();
            deadline.is_some_and(|d| crate::sync::passed(d))
        }
    }
}

/// Does the work of [`wait_until_ready_or`]: returns whether the deadline
/// passed, or None, without blocking, when the descriptors cannot be
/// watched.
unsafe fn block_until_ready(fds: &[libc::pollfd], deadline: Option<Deadline>) -> Option<bool> {
    let Ok(waits) = Epoll::new() else {
        return None;
    };
    for (i, pfd) in fds.iter().enumerate().filter(|(_, pfd)| pfd.fd >= 0) {
        // poll may list a descriptor more than once; epoll takes it once.
        let earlier = fds[..i].iter().any(|other| other.fd == pfd.fd);
        let events = fds[..=i]
            .iter()
            .filter(|other| other.fd == pfd.fd)
            .fold(0, |events, other| events | other.events);
        let interest = Interest::from_poll(events);
        let watched = if earlier {
            waits.modify(pfd.fd, interest, 0)
        } else {
            waits.add(pfd.fd, interest, 0)
        };
        if watched.is_err() {
            return None;
        }
    }
    crate::rsched_glock();
    let caller = crate::my_pt();
    let task = crate::st().task(caller);
    // Mark the task blocked and run other tasks until one of them, or the
    // kernel, makes a descriptor in `waits` ready and the scheduler picks
    // this task again. Like a mutex waiter, it parks if the scheduler had no
    // other task to switch to.
    let mut timed_out = false;
    let waited = crate::task_provider::io_wait(task, &waits, || match deadline {
        Some(deadline) => timed_out = crate::sync::block_until_deadline(caller, deadline),
        None => {
            crate::st().context_switch(caller);
            crate::st().park_if_blocked(caller);
        }
    });
    crate::rsched_gunlock();
    waited.then_some(timed_out)
}

/// Waits, as a scheduling point, until epoll instance `epfd` has an event
/// or `timeout` milliseconds pass, like a modeled `poll` on `epfd`, and
/// returns the timeout for the real `epoll_wait`, which is then zero so the
/// call cannot block in the kernel while the task holds the turn. A
/// negative timeout blocks the task until `epfd` is ready. Returns None when
/// a signal handler interrupted the wait, which then fails with EINTR.
pub(crate) unsafe fn prepare_epoll_wait(
    instr_addr: u64,
    epfd: c_int,
    timeout: c_int,
) -> Option<c_int> {
    crate::ensure_init();
    io_point(instr_addr, epfd, libc::SYS_epoll_wait);
    let deadline = (timeout > 0).then(|| crate::sync::deadline_in_ms(timeout));
    let fds = [libc::pollfd {
        fd: epfd,
        events: libc::POLLIN,
        revents: 0,
    }];
    while timeout != 0 && !poll_one(epfd, libc::POLLIN) {
        if wait_until_ready_or(&fds, deadline) {
            break;
        }
        if crate::signals::interrupts(false) {
            return None;
        }
    }
    Some(0)
}

/// Makes `nr` a scheduling point when `schedule` is set, then blocks until
/// the call can no longer block. May rewrite `args`: a `poll` whose timeout
/// would only elapse while every other task waits gets a zero timeout, so
/// timeouts expire in virtual time. Non-I/O syscalls are left untouched.
/// Returns true, without the call having run, when a signal handler
/// interrupted the wait and the call must fail with EINTR: always for
/// `poll`, and for the other calls when the handler lacks SA_RESTART.
#[must_use]
pub(crate) unsafe fn prepare(
    instr_addr: u64,
    nr: c_long,
    args: &mut [c_long; 6],
    schedule: bool,
) -> bool {
    let Some(blocking) = classify(nr, args) else {
        return false;
    };
    crate::ensure_init();
    if schedule {
        io_point(instr_addr, args[0] as c_int, nr);
    }
    match blocking {
        Blocking::Never => {}
        Blocking::Until(events) => {
            let fd = args[0] as c_int;
            if is_nonblocking(fd) {
                return false;
            }
            while !poll_one(fd, events) {
                wait_until_ready(&[libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                }]);
                if crate::signals::interrupts(true) {
                    return true;
                }
            }
        }
        Blocking::Poll => {
            let (fds, nfds, timeout) = (args[0], args[1], args[2] as c_int);
            let ready = || raw_syscall6(libc::SYS_poll, fds, nfds, 0, 0, 0, 0) != 0;
            if timeout == 0 || ready() {
                return false;
            }
            // poll(NULL, 0, -1) sleeps forever, so it waits on no descriptors.
            let fds_slice = if nfds == 0 {
                &[][..]
            } else {
                core::slice::from_raw_parts(fds as *const libc::pollfd, nfds as usize)
            };
            // A positive timeout is a timed wait: it expires once virtual
            // time reaches it or no task can run, and the real call then
            // reports it with a zero timeout.
            let deadline = (timeout > 0).then(|| crate::sync::deadline_in_ms(timeout));
            while !ready() {
                if wait_until_ready_or(fds_slice, deadline) {
                    args[2] = 0;
                    return false;
                }
                if crate::signals::interrupts(false) {
                    return true;
                }
            }
        }
    }
    false
}

#[inline(always)]
unsafe fn modeled_syscall(instr_addr: u64, nr: c_long, mut args: [c_long; 6]) -> c_long {
    if prepare(instr_addr, nr, &mut args, true) {
        *libc::__errno_location() = libc::EINTR;
        return -1;
    }
    let r = crate::rsched_raw_syscall(nr, args[0], args[1], args[2], args[3], args[4], args[5]);
    crate::files::after_syscall(nr, &args, r);
    r
}

/// Entry point for code that is about to execute a raw I/O `syscall`
/// instruction itself, such as the e9patch hook. `args` holds the six syscall
/// arguments and may be rewritten (see [`prepare`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_io_prepare_syscall(
    instr_addr: *const c_void,
    nr: c_long,
    args: *mut [c_long; 6],
) {
    if !crate::instrumented_libc_ready() || crate::is_in_rsched() {
        return;
    }
    // The hooked instruction runs the syscall afterwards, so an interrupted
    // wait cannot fail it: the handler has run, and the call waits again as
    // if restarted.
    crate::with_libc_wrappers_bypassed(|| while prepare(instr_addr as u64, nr, &mut *args, true) {});
}

/// Routes a modeled syscall issued through `syscall(2)`.
pub(crate) unsafe fn syscall(instr_addr: u64, nr: c_long, args: [c_long; 6]) -> c_long {
    modeled_syscall(instr_addr, nr, args)
}

// ── rsched's reserved descriptors ─────────────────────────────────────────
//
// rsched keeps two adjacent descriptors open for itself (see
// `task_provider::reserved_fd_base`). A program that closes descriptors it did
// not open, as daemons and `closefrom` do, would otherwise close them and
// could then get the same numbers back for its own files. Every call that
// closes or replaces a descriptor therefore leaves the reserved ones alone.

/// Syscalls that can close a descriptor; the seccomp filter traps these.
pub(crate) const FD_GUARDED_SYSCALLS: &[c_long] = &[
    libc::SYS_close,
    libc::SYS_close_range,
    libc::SYS_dup2,
    libc::SYS_dup3,
];

/// rsched's two reserved descriptors, `base` and `base + 1`.
fn reserved() -> Option<core::ops::RangeInclusive<c_long>> {
    crate::task_provider::reserved_fd_base().map(|base| c_long::from(base)..=c_long::from(base) + 1)
}

fn is_reserved(fd: c_long) -> bool {
    reserved().is_some_and(|reserved| reserved.contains(&fd))
}

/// Runs `nr` as a raw syscall without touching rsched's reserved
/// descriptors, returning the raw result (or -errno). Returns `None` for
/// syscalls that cannot close a descriptor.
///
/// - `close` of a reserved descriptor reports success and does nothing.
/// - `close_range` closes, or marks close-on-exec, everything in the range
///   except the reserved descriptors.
/// - `dup2` and `dup3` onto a reserved descriptor fail with `EBADF`, as for a
///   number the process may not use, since they would close it.
pub(crate) unsafe fn guarded_syscall(nr: c_long, args: &[c_long; 6]) -> Option<c_long> {
    let raw = |nr, a0, a1, a2| raw_syscall6(nr, a0, a1, a2, 0, 0, 0);
    Some(match nr {
        libc::SYS_close if is_reserved(args[0]) => 0,
        libc::SYS_close => raw(nr, args[0], 0, 0),
        libc::SYS_close_range => {
            let (first, last) = (args[0] as u32, args[1] as u32);
            let Some(reserved) = reserved().filter(|r| {
                first <= last && *r.start() <= c_long::from(last) && *r.end() >= c_long::from(first)
            }) else {
                return Some(raw(nr, args[0], args[1], args[2]));
            };
            // The range covers a reserved descriptor: close what lies below
            // and above the reserved pair.
            let below = (c_long::from(first), reserved.start() - 1);
            let above = (reserved.end() + 1, c_long::from(last));
            for (low, high) in [below, above] {
                let r = if low <= high {
                    raw(nr, low, high, args[2])
                } else {
                    0
                };
                if r < 0 {
                    return Some(r);
                }
            }
            0
        }
        libc::SYS_dup2 | libc::SYS_dup3 if args[0] != args[1] && is_reserved(args[1]) => {
            -c_long::from(libc::EBADF)
        }
        libc::SYS_dup2 | libc::SYS_dup3 => raw(nr, args[0], args[1], args[2]),
        _ => return None,
    })
}

/// Converts a raw syscall result to libc's -1-and-errno convention.
pub(crate) unsafe fn libc_result(r: c_long) -> c_long {
    if (-4095..0).contains(&r) {
        *libc::__errno_location() = -r as c_int;
        -1
    } else {
        r
    }
}

unsafe fn guarded(nr: c_long, args: [c_long; 6]) -> c_long {
    libc_result(guarded_syscall(nr, &args).expect("rsched: syscall is not fd-guarded"))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_close(fd: c_int) -> c_int {
    guarded(libc::SYS_close, [fd.into(), 0, 0, 0, 0, 0]) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_close_range(
    first: libc::c_uint,
    last: libc::c_uint,
    flags: c_int,
) -> c_int {
    guarded(
        libc::SYS_close_range,
        [first.into(), last.into(), flags.into(), 0, 0, 0],
    ) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_closefrom(lowfd: c_int) {
    rsched_close_range(lowfd.max(0) as libc::c_uint, libc::c_uint::MAX, 0);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_dup2(old: c_int, new: c_int) -> c_int {
    guarded(libc::SYS_dup2, [old.into(), new.into(), 0, 0, 0, 0]) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_dup3(old: c_int, new: c_int, flags: c_int) -> c_int {
    guarded(
        libc::SYS_dup3,
        [old.into(), new.into(), flags.into(), 0, 0, 0],
    ) as c_int
}

macro_rules! arg {
    ($value:expr) => {
        $value as c_long
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    let args = [arg!(fd), arg!(buf), arg!(count), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_read, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    let args = [arg!(fd), arg!(buf), arg!(count), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_write, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_readv(fd: c_int, iov: *const libc::iovec, iovcnt: c_int) -> isize {
    let args = [arg!(fd), arg!(iov), arg!(iovcnt), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_readv, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_writev(fd: c_int, iov: *const libc::iovec, iovcnt: c_int) -> isize {
    let args = [arg!(fd), arg!(iov), arg!(iovcnt), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_writev, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_recv(
    fd: c_int,
    buf: *mut c_void,
    len: usize,
    flags: c_int,
) -> isize {
    let args = [arg!(fd), arg!(buf), arg!(len), arg!(flags), 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_recvfrom, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_recvfrom(
    fd: c_int,
    buf: *mut c_void,
    len: usize,
    flags: c_int,
    addr: *mut libc::sockaddr,
    addrlen: *mut libc::socklen_t,
) -> isize {
    let args = [
        arg!(fd),
        arg!(buf),
        arg!(len),
        arg!(flags),
        arg!(addr),
        arg!(addrlen),
    ];
    modeled_syscall(crate::return_address(), libc::SYS_recvfrom, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_recvmsg(fd: c_int, msg: *mut libc::msghdr, flags: c_int) -> isize {
    let args = [arg!(fd), arg!(msg), arg!(flags), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_recvmsg, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_send(
    fd: c_int,
    buf: *const c_void,
    len: usize,
    flags: c_int,
) -> isize {
    let args = [arg!(fd), arg!(buf), arg!(len), arg!(flags), 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_sendto, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sendto(
    fd: c_int,
    buf: *const c_void,
    len: usize,
    flags: c_int,
    addr: *const libc::sockaddr,
    addrlen: libc::socklen_t,
) -> isize {
    let args = [
        arg!(fd),
        arg!(buf),
        arg!(len),
        arg!(flags),
        arg!(addr),
        arg!(addrlen),
    ];
    modeled_syscall(crate::return_address(), libc::SYS_sendto, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sendmsg(
    fd: c_int,
    msg: *const libc::msghdr,
    flags: c_int,
) -> isize {
    let args = [arg!(fd), arg!(msg), arg!(flags), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_sendmsg, args) as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_accept(
    fd: c_int,
    addr: *mut libc::sockaddr,
    addrlen: *mut libc::socklen_t,
) -> c_int {
    let args = [arg!(fd), arg!(addr), arg!(addrlen), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_accept, args) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_accept4(
    fd: c_int,
    addr: *mut libc::sockaddr,
    addrlen: *mut libc::socklen_t,
    flags: c_int,
) -> c_int {
    let args = [arg!(fd), arg!(addr), arg!(addrlen), arg!(flags), 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_accept4, args) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_connect(
    fd: c_int,
    addr: *const libc::sockaddr,
    addrlen: libc::socklen_t,
) -> c_int {
    let args = [arg!(fd), arg!(addr), arg!(addrlen), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_connect, args) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_poll(
    fds: *mut libc::pollfd,
    nfds: libc::nfds_t,
    timeout: c_int,
) -> c_int {
    let args = [arg!(fds), arg!(nfds), arg!(timeout), 0, 0, 0];
    modeled_syscall(crate::return_address(), libc::SYS_poll, args) as c_int
}
