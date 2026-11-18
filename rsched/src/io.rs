//! File and socket operations as scheduling points.
//!
//! Only one task runs at a time, so an I/O call that blocks in the kernel
//! would stall every other task, including the one that would unblock it.
//! Each modeled operation is therefore a scheduling point followed, for a
//! blocking descriptor, by a loop that yields until the descriptor is ready.
//! The operation itself then runs as a raw syscall that cannot block.
//!
//! Every integration mode funnels through [`prepare`]: the `rsched_*`
//! wrappers used by source annotations, the LLVM pass, and instrumented libc;
//! libc's `syscall()` trampoline; the e9patch hook for raw `syscall`
//! instructions; and the seccomp handler for I/O issued outside rsched.

use crate::event::{Event, EventKind};
use crate::seccomp::raw_syscall6;
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

unsafe fn poll_one(fd: c_int, events: libc::c_short) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // Ready, hung up, or an error the real call will report.
    raw_syscall6(libc::SYS_poll, &mut pfd as *mut _ as c_long, 1, 0, 0, 0, 0) != 0
}

unsafe fn is_nonblocking(fd: c_int) -> bool {
    let flags = raw_syscall6(
        libc::SYS_fcntl,
        c_long::from(fd),
        c_long::from(libc::F_GETFL),
        0,
        0,
        0,
        0,
    );
    // A bad descriptor fails immediately in the real call.
    flags < 0 || flags & c_long::from(libc::O_NONBLOCK) != 0
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

/// Makes `nr` a scheduling point when `schedule` is set, then yields until
/// the call can no longer block. May rewrite `args`: a `poll` whose timeout
/// would only elapse while every other task waits gets a zero timeout, so
/// timeouts expire in virtual time. Non-I/O syscalls are left untouched.
pub(crate) unsafe fn prepare(instr_addr: u64, nr: c_long, args: &mut [c_long; 6], schedule: bool) {
    let Some(blocking) = classify(nr, args) else {
        return;
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
                return;
            }
            while !poll_one(fd, events) {
                crate::rsched_sched_yield();
            }
        }
        Blocking::Poll => {
            let (fds, nfds, timeout) = (args[0], args[1], args[2] as c_int);
            let ready = || raw_syscall6(libc::SYS_poll, fds, nfds, 0, 0, 0, 0) != 0;
            if timeout == 0 || ready() {
                return;
            }
            if timeout > 0 {
                crate::rsched_sched_yield();
                if !ready() {
                    args[2] = 0;
                }
                return;
            }
            while !ready() {
                crate::rsched_sched_yield();
            }
        }
    }
}

#[inline(always)]
unsafe fn modeled_syscall(instr_addr: u64, nr: c_long, mut args: [c_long; 6]) -> c_long {
    prepare(instr_addr, nr, &mut args, true);
    crate::rsched_raw_syscall(nr, args[0], args[1], args[2], args[3], args[4], args[5])
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
    crate::with_libc_wrappers_bypassed(|| prepare(instr_addr as u64, nr, &mut *args, true));
}

/// Routes a modeled syscall issued through `syscall(2)`.
pub(crate) unsafe fn syscall(instr_addr: u64, nr: c_long, args: [c_long; 6]) -> c_long {
    modeled_syscall(instr_addr, nr, args)
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
