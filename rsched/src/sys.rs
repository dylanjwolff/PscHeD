//! Safe wrappers for the descriptor and file syscalls rsched makes itself.
//!
//! Every call goes through rsched's own syscall instruction,
//! [`raw_syscall6`]: libc's functions could re-enter rsched's instrumented
//! libc wrappers, and the seccomp filter traps some of these syscalls
//! (`close`, for one) unless they come from that instruction. Errors are
//! returned as [`io::Error`]s built from the errno, which do not allocate.

use crate::seccomp::raw_syscall6;
use libc::{c_int, c_long};
use std::ffi::CStr;
use std::io;
use std::mem::ManuallyDrop;
use std::ops::BitOr;

/// Runs `nr` and turns a -errno result into an error.
fn syscall(nr: c_long, a0: c_long, a1: c_long, a2: c_long, a3: c_long) -> io::Result<c_long> {
    // SAFETY: callers pass only arguments the kernel reads as integers or as
    // pointers into memory they borrow for the duration of the call.
    let r = unsafe { raw_syscall6(nr, a0, a1, a2, a3, 0, 0) };
    if (-4095..0).contains(&r) {
        Err(io::Error::from_raw_os_error(-r as c_int))
    } else {
        Ok(r)
    }
}

/// Closes `fd`. Closing a descriptor someone else still uses is a logic
/// error but not memory-unsafe, as with `close(2)` itself.
pub(crate) fn close(fd: c_int) -> io::Result<()> {
    syscall(libc::SYS_close, fd.into(), 0, 0, 0).map(drop)
}

/// Duplicates `fd` to the lowest free number of at least `min`. The copy
/// stays open across exec.
pub(crate) fn dup_from(fd: c_int, min: c_int) -> io::Result<c_int> {
    syscall(
        libc::SYS_fcntl,
        fd.into(),
        libc::F_DUPFD.into(),
        min.into(),
        0,
    )
    .map(|fd| fd as c_int)
}

/// The file status flags of `fd`, such as `O_NONBLOCK`.
pub(crate) fn status_flags(fd: c_int) -> io::Result<c_int> {
    syscall(libc::SYS_fcntl, fd.into(), libc::F_GETFL.into(), 0, 0).map(|flags| flags as c_int)
}

/// Creates an anonymous memory file named `name`, open across exec.
pub(crate) fn memfd_create(name: &std::ffi::CStr) -> io::Result<c_int> {
    syscall(libc::SYS_memfd_create, name.as_ptr() as c_long, 0, 0, 0).map(|fd| fd as c_int)
}

/// The events a descriptor is watched for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interest(u32);

impl Interest {
    /// Readable, or for an epoll, has events to report.
    pub(crate) const IN: Self = Self(libc::EPOLLIN as u32);
    /// Reports the descriptor once, then disables it until it is modified.
    pub(crate) const ONESHOT: Self = Self(libc::EPOLLONESHOT as u32);

    /// The same events as `poll`'s `events` bits, whose values epoll shares.
    pub(crate) fn from_poll(events: libc::c_short) -> Self {
        Self(events as u16 as u32)
    }
}

impl BitOr for Interest {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One ready descriptor reported by [`Epoll::wait`].
#[derive(Clone, Copy)]
#[repr(transparent)]
pub(crate) struct Event(libc::epoll_event);

impl Event {
    pub(crate) const EMPTY: Self = Self(libc::epoll_event { events: 0, u64: 0 });

    /// The data given when the descriptor was added.
    pub(crate) fn data(&self) -> u64 {
        self.0.u64
    }
}

/// How long [`Epoll::wait`] waits for an event.
#[derive(Clone, Copy)]
pub(crate) enum Timeout {
    /// Reports what is ready now without waiting.
    Now,
    /// Waits until something is ready.
    Never,
}

/// An epoll instance, closed when dropped.
pub(crate) struct Epoll {
    fd: c_int,
}

impl Epoll {
    /// Creates an epoll instance that is closed on exec.
    pub(crate) fn new() -> io::Result<Self> {
        let flags = libc::EPOLL_CLOEXEC.into();
        syscall(libc::SYS_epoll_create1, flags, 0, 0, 0).map(|fd| Self { fd: fd as c_int })
    }

    /// Uses an epoll instance that something else keeps open, without
    /// closing it.
    ///
    /// # Safety
    ///
    /// `fd` must be an epoll instance that stays open while the result is
    /// used.
    pub(crate) unsafe fn borrow_raw(fd: c_int) -> ManuallyDrop<Self> {
        ManuallyDrop::new(Self { fd })
    }

    pub(crate) fn as_raw_fd(&self) -> c_int {
        self.fd
    }

    /// Starts watching `fd` for `interest`; [`wait`](Self::wait) then
    /// reports `data` for it.
    pub(crate) fn add(&self, fd: c_int, interest: Interest, data: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, interest, data)
    }

    /// Changes what an added `fd` is watched for.
    pub(crate) fn modify(&self, fd: c_int, interest: Interest, data: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, interest, data)
    }

    /// Stops watching `fd`.
    pub(crate) fn delete(&self, fd: c_int) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_DEL, fd, Interest(0), 0)
    }

    fn ctl(&self, op: c_int, fd: c_int, interest: Interest, data: u64) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: interest.0,
            u64: data,
        };
        let event = &mut event as *mut libc::epoll_event as c_long;
        syscall(
            libc::SYS_epoll_ctl,
            self.fd.into(),
            op.into(),
            fd.into(),
            event,
        )
        .map(drop)
    }

    /// Fills `events` with ready descriptors, waiting as `timeout` says, and
    /// returns the filled part.
    pub(crate) fn wait<'e>(
        &self,
        events: &'e mut [Event],
        timeout: Timeout,
    ) -> io::Result<&'e [Event]> {
        let timeout_ms = match timeout {
            Timeout::Now => 0,
            Timeout::Never => -1,
        };
        let max = events.len().min(c_int::MAX as usize) as c_long;
        // `Event` is a transparent `epoll_event`, and the kernel writes at
        // most `max` of them.
        let buffer = events.as_mut_ptr() as c_long;
        let ready = syscall(
            libc::SYS_epoll_wait,
            self.fd.into(),
            buffer,
            max,
            timeout_ms,
        )?;
        Ok(&events[..ready as usize])
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

// ── File metadata, for the virtual file times in `files` ──────────────────

/// The kernel's `struct statx`; the `libc` crate has none for musl.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Statx {
    pub(crate) mask: u32,
    blksize: u32,
    attributes: u64,
    nlink: u32,
    uid: u32,
    gid: u32,
    mode: u16,
    spare0: u16,
    pub(crate) ino: u64,
    size: u64,
    blocks: u64,
    attributes_mask: u64,
    pub(crate) atime: StatxTimestamp,
    btime: StatxTimestamp,
    pub(crate) ctime: StatxTimestamp,
    pub(crate) mtime: StatxTimestamp,
    rdev_major: u32,
    rdev_minor: u32,
    pub(crate) dev_major: u32,
    pub(crate) dev_minor: u32,
    spare: [u64; 14],
}

#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct StatxTimestamp {
    pub(crate) sec: i64,
    pub(crate) nsec: u32,
    reserved: i32,
}

pub(crate) const STATX_ATIME: u32 = 0x20;
pub(crate) const STATX_MTIME: u32 = 0x40;
pub(crate) const STATX_CTIME: u32 = 0x80;
pub(crate) const STATX_INO: u32 = 0x100;

/// The metadata of the file open as `fd`.
pub(crate) fn fstat(fd: c_int) -> io::Result<libc::stat> {
    // SAFETY: all-zero is a valid `stat`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let buf = &mut st as *mut libc::stat as c_long;
    syscall(libc::SYS_fstat, fd.into(), buf, 0, 0)?;
    Ok(st)
}

/// The metadata of `path`, relative to `dirfd`, as `fstatat` reports it.
pub(crate) fn fstatat(dirfd: c_int, path: &CStr, flags: c_int) -> io::Result<libc::stat> {
    // SAFETY: all-zero is a valid `stat`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let buf = &mut st as *mut libc::stat as c_long;
    let path = path.as_ptr() as c_long;
    syscall(libc::SYS_newfstatat, dirfd.into(), path, buf, flags.into())?;
    Ok(st)
}

/// The metadata of `path`, relative to `dirfd`, as `statx` reports it.
pub(crate) fn statx(dirfd: c_int, path: &CStr, flags: c_int, mask: u32) -> io::Result<Statx> {
    // SAFETY: all-zero is a valid `Statx`.
    let mut stx: Statx = unsafe { std::mem::zeroed() };
    let buf = &mut stx as *mut Statx as c_long;
    let path = path.as_ptr() as c_long;
    // SAFETY: as in `syscall`; statx takes a fifth argument.
    let r = unsafe {
        raw_syscall6(
            libc::SYS_statx,
            dirfd.into(),
            path,
            flags.into(),
            mask.into(),
            buf,
            0,
        )
    };
    if (-4095..0).contains(&r) {
        return Err(io::Error::from_raw_os_error(-r as c_int));
    }
    Ok(stx)
}

/// Sets the length of the file open as `fd`.
pub(crate) fn ftruncate(fd: c_int, length: libc::off_t) -> io::Result<()> {
    syscall(libc::SYS_ftruncate, fd.into(), length, 0, 0).map(drop)
}

/// Sets the length of the file at `path`.
pub(crate) fn truncate(path: &CStr, length: libc::off_t) -> io::Result<()> {
    syscall(libc::SYS_truncate, path.as_ptr() as c_long, length, 0, 0).map(drop)
}
