//! Virtual file timestamps.
//!
//! The kernel stamps a file's times with the real clock, which under rsched
//! disagrees with the virtual clock (see `virt`): a program that writes a
//! file, sleeps, and compares the file's mtime with `time()` would see the
//! real time barely move. rsched keeps a table, in process-shared memory, of
//! the access, modification, and change times of files keyed by device and
//! inode, and the stat calls report those instead of the kernel's.
//!
//! - A write, `truncate`, `ftruncate`, or `fallocate` that succeeds sets the
//!   file's mtime and ctime to the current virtual time.
//! - Any other change, which rsched does not see, such as creating a file or
//!   a write through `mmap`, shows up as a kernel time newer than the one the
//!   table recorded. rsched then reports the virtual time at which the stat
//!   call observed it; a kernel time from before rsched started, such as on a
//!   file that already existed, is reported unchanged.
//!
//! The table holds [`SLOTS`] files; once it is full, a file outside it gets
//! the current virtual time for each time that changed after rsched started.
//! Times set explicitly with `utimensat` count as such a change: a date
//! after rsched started is reported as the virtual time of the stat call
//! that saw it.

use libc::{c_int, c_long};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

const SLOTS: usize = 4096;

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

/// The kernel's `struct statx` up to the device numbers; the `libc` crate
/// has none for musl. Only read through a pointer to the caller's buffer.
#[repr(C)]
pub struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    __spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTimestamp,
    stx_btime: StatxTimestamp,
    stx_ctime: StatxTimestamp,
    stx_mtime: StatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
}

#[repr(C)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    __reserved: i32,
}

const STATX_ATIME: u32 = 0x20;
const STATX_MTIME: u32 = 0x40;
const STATX_CTIME: u32 = 0x80;
const STATX_INO: u32 = 0x100;

fn table() -> &'static SharedFiles {
    unsafe { &*crate::task_provider::shared_files() }
}

fn slot(dev: u64, ino: u64) -> Option<(&'static Slot, bool)> {
    let key = ino.wrapping_add(1);
    let start = (ino ^ dev.rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) as usize;
    for probe in 0..SLOTS {
        let s = &table().slots[(start.wrapping_add(probe)) % SLOTS];
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

/// The time to report for a kernel time the table has not recorded.
fn first_seen(real: i64, now: i64) -> i64 {
    if real < crate::virt::REALTIME_BASE_NS {
        real
    } else {
        now
    }
}

/// Records the file's kernel times `real` and returns the times to report.
/// `stamp` lists the times a write just set.
fn observe(dev: u64, ino: u64, real: Times, stamp: &[usize]) -> Times {
    let now = crate::virt::realtime_now_ns();
    let Some((s, new)) = slot(dev, ino) else {
        return real.map(|t| first_seen(t, now));
    };
    let mut out = [0; 3];
    for i in 0..3 {
        if stamp.contains(&i) {
            s.virt[i].store(now, Ordering::Relaxed);
        } else if new || s.real[i].load(Ordering::Relaxed) != real[i] {
            s.virt[i].store(first_seen(real[i], now), Ordering::Relaxed);
        }
        s.real[i].store(real[i], Ordering::Relaxed);
        out[i] = s.virt[i].load(Ordering::Relaxed);
    }
    out
}

fn ns(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(1_000_000_000).saturating_add(nsec)
}

fn stat_times(st: &libc::stat) -> Times {
    [
        ns(st.st_atime, st.st_atime_nsec),
        ns(st.st_mtime, st.st_mtime_nsec),
        ns(st.st_ctime, st.st_ctime_nsec),
    ]
}

unsafe fn fix_stat(st: *mut libc::stat) {
    let st = &mut *st;
    let t = observe(st.st_dev, st.st_ino, stat_times(st), &[]);
    (st.st_atime, st.st_atime_nsec) = (
        t[0].div_euclid(1_000_000_000),
        t[0].rem_euclid(1_000_000_000),
    );
    (st.st_mtime, st.st_mtime_nsec) = (
        t[1].div_euclid(1_000_000_000),
        t[1].rem_euclid(1_000_000_000),
    );
    (st.st_ctime, st.st_ctime_nsec) = (
        t[2].div_euclid(1_000_000_000),
        t[2].rem_euclid(1_000_000_000),
    );
}

unsafe fn fix_statx(stx: *mut Statx) {
    let stx = &mut *stx;
    let wanted = STATX_ATIME | STATX_MTIME | STATX_CTIME | STATX_INO;
    if stx.stx_mask & wanted != wanted {
        return;
    }
    let dev = libc::makedev(stx.stx_dev_major, stx.stx_dev_minor);
    let fields = [&mut stx.stx_atime, &mut stx.stx_mtime, &mut stx.stx_ctime];
    let real = fields
        .each_ref()
        .map(|t| ns(t.tv_sec, i64::from(t.tv_nsec)));
    let t = observe(dev, stx.stx_ino, real, &[]);
    for (field, t) in fields.into_iter().zip(t) {
        field.tv_sec = t.div_euclid(1_000_000_000);
        field.tv_nsec = t.rem_euclid(1_000_000_000) as u32;
    }
}

/// Sets the mtime and ctime of the file `st` describes to now.
fn stamp(st: &libc::stat) {
    observe(st.st_dev, st.st_ino, stat_times(st), &[MTIME, CTIME]);
}

unsafe fn stamp_fd(fd: c_long) {
    let mut st: libc::stat = core::mem::zeroed();
    if crate::seccomp::raw_syscall6(libc::SYS_fstat, fd, &mut st as *mut _ as c_long, 0, 0, 0, 0)
        == 0
    {
        stamp(&st);
    }
}

unsafe fn stamp_path(path: c_long) {
    let mut st: libc::stat = core::mem::zeroed();
    let at_fdcwd = c_long::from(libc::AT_FDCWD);
    let buf = &mut st as *mut _ as c_long;
    if crate::seccomp::raw_syscall6(libc::SYS_newfstatat, at_fdcwd, path, buf, 0, 0, 0) == 0 {
        stamp(&st);
    }
}

/// Updates the table after syscall `nr` returned `ret`, which is a byte or
/// zero count on success and negative on failure: a write stamps its file,
/// and a stat call gets the table's times. Ignores other syscalls.
pub(crate) unsafe fn after_syscall(nr: c_long, args: &[c_long; 6], ret: c_long) {
    if ret < 0 || !crate::virt::started() {
        return;
    }
    match nr {
        libc::SYS_write
        | libc::SYS_pwrite64
        | libc::SYS_writev
        | libc::SYS_pwritev
        | libc::SYS_pwritev2
        | libc::SYS_sendfile
            if ret > 0 =>
        {
            stamp_fd(args[0])
        }
        libc::SYS_copy_file_range | libc::SYS_splice if ret > 0 => stamp_fd(args[2]),
        libc::SYS_ftruncate | libc::SYS_fallocate => stamp_fd(args[0]),
        libc::SYS_truncate => stamp_path(args[0]),
        libc::SYS_stat | libc::SYS_fstat | libc::SYS_lstat => fix_stat(args[1] as *mut _),
        libc::SYS_newfstatat => fix_stat(args[2] as *mut _),
        libc::SYS_statx => fix_statx(args[4] as *mut _),
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

/// `stat` and friends for programs built against rsched: the raw syscall,
/// with the table's times.
unsafe fn stat_call(nr: c_long, args: [c_long; 6]) -> c_int {
    let r = crate::seccomp::raw_syscall6(nr, args[0], args[1], args[2], args[3], args[4], args[5]);
    if r < 0 {
        *libc::__errno_location() = -r as c_int;
        return -1;
    }
    after_syscall(nr, &args, r);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_stat(path: *const libc::c_char, buf: *mut libc::stat) -> c_int {
    stat_call(
        libc::SYS_newfstatat,
        [
            libc::AT_FDCWD as c_long,
            path as c_long,
            buf as c_long,
            0,
            0,
            0,
        ],
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_lstat(path: *const libc::c_char, buf: *mut libc::stat) -> c_int {
    let flags = libc::AT_SYMLINK_NOFOLLOW as c_long;
    stat_call(
        libc::SYS_newfstatat,
        [
            libc::AT_FDCWD as c_long,
            path as c_long,
            buf as c_long,
            flags,
            0,
            0,
        ],
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
    stat_call(libc::SYS_fstat, [fd as c_long, buf as c_long, 0, 0, 0, 0])
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fstatat(
    dirfd: c_int,
    path: *const libc::c_char,
    buf: *mut libc::stat,
    flags: c_int,
) -> c_int {
    stat_call(
        libc::SYS_newfstatat,
        [
            dirfd as c_long,
            path as c_long,
            buf as c_long,
            flags as c_long,
            0,
            0,
        ],
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_statx(
    dirfd: c_int,
    path: *const libc::c_char,
    flags: c_int,
    mask: libc::c_uint,
    buf: *mut Statx,
) -> c_int {
    let args = [
        dirfd as c_long,
        path as c_long,
        flags as c_long,
        mask as c_long,
        buf as c_long,
        0,
    ];
    stat_call(libc::SYS_statx, args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_ftruncate(fd: c_int, length: libc::off_t) -> c_int {
    let r = crate::rsched_raw_syscall(libc::SYS_ftruncate, fd as c_long, length, 0, 0, 0, 0);
    after_syscall(libc::SYS_ftruncate, &[fd as c_long, 0, 0, 0, 0, 0], r);
    r as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_truncate(path: *const libc::c_char, length: libc::off_t) -> c_int {
    let r = crate::rsched_raw_syscall(libc::SYS_truncate, path as c_long, length, 0, 0, 0, 0);
    after_syscall(libc::SYS_truncate, &[path as c_long, 0, 0, 0, 0, 0], r);
    r as c_int
}
