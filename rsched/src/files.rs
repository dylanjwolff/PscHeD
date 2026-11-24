//! Virtual file timestamps.
//!
//! The kernel stamps a file's times with the real clock, which under rsched
//! disagrees with the virtual clock (see `virt`): a program that writes a
//! file, sleeps, and compares the file's mtime with `time()` would see the
//! wrong order. rsched keeps a table, in process-shared memory, of the
//! access, modification, and change times of files keyed by device and
//! inode, and the stat calls report those instead of the kernel's.
//!
//! - A write, `truncate`, `ftruncate`, or `fallocate` that succeeds sets the
//!   file's mtime and ctime to the current virtual time.
//! - A file the table has not seen yet reports the virtual epoch
//!   ([`virt::REALTIME_BASE_NS`]) for every time, so files that existed
//!   before rsched started look older than anything it stamps.
//! - Any other change to a recorded file, which rsched does not see, such as
//!   a write through `mmap` or `utimensat`, shows up as a kernel time that
//!   differs from the one the table recorded, and is reported as the virtual
//!   time of the stat call that saw it.
//!
//! The table holds [`SLOTS`] files; a file outside a full table reports the
//! virtual epoch. All syscalls go through the safe wrappers in `sys`.

use crate::sys::{self, STATX_ATIME, STATX_CTIME, STATX_INO, STATX_MTIME, Statx};
use crate::virt;
use libc::{c_char, c_int, c_long};
use std::ffi::CStr;
use std::io;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

const SLOTS: usize = 4096;

/// 2^64 divided by the golden ratio, for Fibonacci hashing: multiplying by
/// it spreads consecutive inode numbers, which file systems often hand out,
/// across the table instead of into neighboring slots.
const FIBONACCI_HASH: u64 = 0x9e37_79b9_7f4a_7c15;
const NS: i64 = 1_000_000_000;

/// Atime, mtime, ctime.
type Times = [i64; 3];
const MTIME: usize = 1;
const CTIME: usize = 2;

#[repr(C)]
struct Slot {
    /// `ino + 1`, or zero while the slot is empty.
    ino: AtomicU64,
    dev: AtomicU64,
    /// The kernel's times when the table last recorded the file.
    real: [AtomicI64; 3],
    /// The times rsched reports.
    virt: [AtomicI64; 3],
}

/// Zero-initialized in process-shared memory. Only one task runs at a time,
/// so the table needs no lock.
#[repr(C)]
pub(crate) struct SharedFiles {
    slots: [Slot; SLOTS],
}

fn table() -> &'static SharedFiles {
    // SAFETY: the process-shared mapping lives as long as the process.
    unsafe { &*crate::task_provider::shared_files() }
}

/// The file's slot, and whether it was just claimed.
fn slot(dev: u64, ino: u64) -> Option<(&'static Slot, bool)> {
    let key = ino.wrapping_add(1);
    let start = (ino ^ dev.rotate_left(32)).wrapping_mul(FIBONACCI_HASH) as usize;
    for probe in 0..SLOTS {
        let s = &table().slots[start.wrapping_add(probe) % SLOTS];
        match s.ino.load(Ordering::Relaxed) {
            0 => {
                s.dev.store(dev, Ordering::Relaxed);
                s.ino.store(key, Ordering::Relaxed);
                return Some((s, true));
            }
            k if k == key && s.dev.load(Ordering::Relaxed) == dev => return Some((s, false)),
            _ => {}
        }
    }
    None
}

/// Records the file's kernel times `real` and returns the times to report.
/// `stamp` lists the times a write just set.
fn observe(dev: u64, ino: u64, real: Times, stamp: &[usize]) -> Times {
    if !virt::started() {
        return real;
    }
    let now = virt::realtime_now_ns();
    let Some((s, new)) = slot(dev, ino) else {
        return [virt::REALTIME_BASE_NS; 3];
    };
    let mut out = [0; 3];
    for i in 0..3 {
        if stamp.contains(&i) {
            s.virt[i].store(now, Ordering::Relaxed);
        } else if new {
            s.virt[i].store(virt::REALTIME_BASE_NS, Ordering::Relaxed);
        } else if s.real[i].load(Ordering::Relaxed) != real[i] {
            s.virt[i].store(now, Ordering::Relaxed);
        }
        s.real[i].store(real[i], Ordering::Relaxed);
        out[i] = s.virt[i].load(Ordering::Relaxed);
    }
    out
}

fn ns(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(NS).saturating_add(nsec)
}

fn split(t: i64) -> (i64, i64) {
    (t.div_euclid(NS), t.rem_euclid(NS))
}

fn stat_times(st: &libc::stat) -> Times {
    [
        ns(st.st_atime, st.st_atime_nsec),
        ns(st.st_mtime, st.st_mtime_nsec),
        ns(st.st_ctime, st.st_ctime_nsec),
    ]
}

/// Replaces the kernel's times in `st` with the table's.
fn fix_stat(st: &mut libc::stat) {
    let t = observe(st.st_dev, st.st_ino, stat_times(st), &[]);
    (st.st_atime, st.st_atime_nsec) = split(t[0]);
    (st.st_mtime, st.st_mtime_nsec) = split(t[1]);
    (st.st_ctime, st.st_ctime_nsec) = split(t[2]);
}

/// Replaces the kernel's times in `stx` with the table's, when it has them.
fn fix_statx(stx: &mut Statx) {
    let wanted = STATX_ATIME | STATX_MTIME | STATX_CTIME | STATX_INO;
    if stx.stx_mask & wanted != wanted {
        return;
    }
    let dev = libc::makedev(stx.stx_dev_major, stx.stx_dev_minor);
    let real =
        [stx.stx_atime, stx.stx_mtime, stx.stx_ctime].map(|t| ns(t.tv_sec, i64::from(t.tv_nsec)));
    let t = observe(dev, stx.stx_ino, real, &[]);
    for (field, t) in [&mut stx.stx_atime, &mut stx.stx_mtime, &mut stx.stx_ctime]
        .into_iter()
        .zip(t)
    {
        let (sec, nsec) = split(t);
        field.tv_sec = sec;
        field.tv_nsec = nsec as u32;
    }
}

/// Sets the mtime and ctime of the file `st` describes to now.
fn stamp(st: &libc::stat) {
    observe(st.st_dev, st.st_ino, stat_times(st), &[MTIME, CTIME]);
}

fn stamp_fd(fd: c_int) {
    if let Ok(st) = sys::fstat(fd) {
        stamp(&st);
    }
}

fn stamp_path(path: &CStr) {
    if let Ok(st) = sys::fstatat(libc::AT_FDCWD, path, 0) {
        stamp(&st);
    }
}

/// Updates the table after syscall `nr` returned `ret`, which is a byte or
/// zero count on success and negative on failure: a write stamps its file,
/// and a stat call gets the table's times. Ignores other syscalls.
///
/// # Safety
///
/// `args` must be the arguments of that call, so that after a success its
/// pointers are valid.
pub(crate) unsafe fn after_syscall(nr: c_long, args: &[c_long; 6], ret: c_long) {
    if ret < 0 || !virt::started() {
        return;
    }
    let fd = |i: usize| args[i] as c_int;
    match nr {
        libc::SYS_write
        | libc::SYS_pwrite64
        | libc::SYS_writev
        | libc::SYS_pwritev
        | libc::SYS_pwritev2
        | libc::SYS_sendfile
            if ret > 0 =>
        {
            stamp_fd(fd(0))
        }
        libc::SYS_copy_file_range | libc::SYS_splice if ret > 0 => stamp_fd(fd(2)),
        libc::SYS_ftruncate | libc::SYS_fallocate => stamp_fd(fd(0)),
        libc::SYS_truncate => stamp_path(CStr::from_ptr(args[0] as *const c_char)),
        libc::SYS_stat | libc::SYS_fstat | libc::SYS_lstat => {
            fix_stat(&mut *(args[1] as *mut libc::stat))
        }
        libc::SYS_newfstatat => fix_stat(&mut *(args[2] as *mut libc::stat)),
        libc::SYS_statx => fix_statx(&mut *(args[4] as *mut Statx)),
        _ => {}
    }
}

/// Called by libc after a file function returns `ret`. `nr` names the
/// syscall the function makes, and `args` holds the function's first six
/// arguments, which match that syscall's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_file_call_done(nr: c_long, ret: c_long, args: *const c_long) {
    if crate::is_in_rsched() {
        return;
    }
    after_syscall(nr, &*(args as *const [c_long; 6]), ret);
}

// ── Entry points for programs built against rsched ────────────────────────

/// Stores `value` in `*out` and returns 0, or sets errno and returns -1.
/// A null pointer fails with `EFAULT`, as in the kernel.
unsafe fn report<T>(result: io::Result<T>, out: *mut T) -> c_int {
    match result {
        Ok(_) if out.is_null() => fail(libc::EFAULT),
        Ok(value) => {
            out.write(value);
            0
        }
        Err(e) => fail(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

unsafe fn fail(errno: c_int) -> c_int {
    *libc::__errno_location() = errno;
    -1
}

/// `path` as a C string, or None when it is null.
unsafe fn c_path<'a>(path: *const c_char) -> Option<&'a CStr> {
    (!path.is_null()).then(|| CStr::from_ptr(path))
}

unsafe fn stat_at(dirfd: c_int, path: *const c_char, buf: *mut libc::stat, flags: c_int) -> c_int {
    let Some(path) = c_path(path) else {
        return fail(libc::EFAULT);
    };
    let result = sys::fstatat(dirfd, path, flags).map(|mut st| {
        fix_stat(&mut st);
        st
    });
    report(result, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    stat_at(libc::AT_FDCWD, path, buf, 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    stat_at(libc::AT_FDCWD, path, buf, libc::AT_SYMLINK_NOFOLLOW)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat,
    flags: c_int,
) -> c_int {
    stat_at(dirfd, path, buf, flags)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
    let result = sys::fstat(fd).map(|mut st| {
        fix_stat(&mut st);
        st
    });
    report(result, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_statx(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mask: libc::c_uint,
    buf: *mut Statx,
) -> c_int {
    let Some(path) = c_path(path) else {
        return fail(libc::EFAULT);
    };
    let result = sys::statx(dirfd, path, flags, mask).map(|mut stx| {
        fix_statx(&mut stx);
        stx
    });
    report(result, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_ftruncate(fd: c_int, length: libc::off_t) -> c_int {
    match sys::ftruncate(fd, length) {
        Ok(()) => {
            stamp_fd(fd);
            0
        }
        Err(e) => fail(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_truncate(path: *const c_char, length: libc::off_t) -> c_int {
    let Some(path) = c_path(path) else {
        return fail(libc::EFAULT);
    };
    match sys::truncate(path, length) {
        Ok(()) => {
            stamp_path(path);
            0
        }
        Err(e) => fail(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}
