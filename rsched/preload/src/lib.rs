//! LD_PRELOAD shim for rsched.
//!
//! Build with `cargo build -p rsched-preload` to produce `librsched_preload.so`.
//! Then run a program with:
//!
//!   LD_PRELOAD=/path/to/librsched_preload.so ./my_program
//!
//! Every pthread_create / pthread_join / pthread_mutex_* / pthread_cond_* /
//! pthread_barrier_* / sched_yield call in the target binary is redirected
//! to the rsched cooperative scheduler.
//!
//! This library intentionally exports ONLY the pthread-named symbols.
//! Programs that want static linking should link against librsched.a instead
//! and use the rsched.h header (which #define-redirects calls at compile time).
//!
//! Reentrancy handling: each exported function increments CALL_DEPTH on entry.
//! If depth > 0 on entry (i.e., we are already inside rsched), the call is a
//! re-entrant call (e.g. from TSAN's internal background thread, or from rsched's
//! own internals via PLT).  In that case we bypass rsched entirely and forward
//! directly to the next implementation via dlsym(RTLD_NEXT, …).

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_safety_doc)]

use scopeguard::defer;
use std::sync::atomic::{AtomicUsize, Ordering};

use libc::{
    RTLD_NEXT, c_int, c_uint, c_void, pthread_attr_t, pthread_barrier_t, pthread_barrierattr_t,
    pthread_cond_t, pthread_mutex_t, pthread_mutexattr_t, pthread_t, timespec,
};

use rsched::{
    rsched_after_fork_child, rsched_after_fork_parent, rsched_before_fork, rsched_execv,
    rsched_execve, rsched_exit, rsched_note_pthread_mutex_destroy, rsched_note_pthread_mutex_init,
    rsched_note_pthread_mutexattr_destroy, rsched_note_pthread_mutexattr_init,
    rsched_note_pthread_mutexattr_settype, rsched_process_exit, rsched_pthread_barrier_init,
    rsched_pthread_barrier_wait, rsched_pthread_cond_broadcast, rsched_pthread_cond_signal,
    rsched_pthread_cond_wait, rsched_pthread_create, rsched_pthread_exit, rsched_pthread_join,
    rsched_pthread_mutex_lock, rsched_pthread_mutex_trylock, rsched_pthread_mutex_unlock,
    rsched_raw_syscall, rsched_sched_yield, rsched_syscall, rsched_try_enter, rsched_waitpid,
};

#[cfg(feature = "tsan")]
use rsched::{rsched_is_tsan_background_start, rsched_is_tsan_thread_start};

// ── Reentrancy depth tracking ─────────────────────────────────────────────────
//
// CALL_DEPTH lives in rsched (the rlib linked into this DSO).  Each interceptor
// calls rsched_try_enter() on entry and rsched_exit() on exit via defer!().
// rsched's own trampoline() and do_thread_exit() call depth_enter/exit() directly
// on the same TLS slot, so all re-entrancy checks are consistent.

// ── RTLD_NEXT resolution diagnostic ──────────────────────────────────────────

/// Print where `dlsym(RTLD_NEXT, name)` resolves for each intercepted symbol.
/// Call this from a test binary (linked against librsched_preload.so) to
/// verify whether RTLD_NEXT lands in the real libc or in an ASAN interceptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_diagnose_rtld_next() {
    let names: &[&[u8]] = &[
        b"pthread_create\0",
        b"pthread_join\0",
        b"pthread_mutex_lock\0",
        b"pthread_mutex_unlock\0",
        b"pthread_cond_wait\0",
        b"pthread_cond_signal\0",
        b"pthread_cond_broadcast\0",
        b"pthread_barrier_init\0",
        b"pthread_barrier_wait\0",
        b"sched_yield\0",
    ];
    eprintln!("[rsched] RTLD_NEXT resolutions from librsched_preload.so:");
    for name in names {
        let sym_name = core::str::from_utf8(&name[..name.len() - 1]).unwrap_or("?");
        let p = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const _);
        if p.is_null() {
            eprintln!("  {:32} → NULL", sym_name);
            continue;
        }
        let mut info: libc::Dl_info = core::mem::zeroed();
        if libc::dladdr(p, &mut info) != 0 && !info.dli_fname.is_null() {
            let fname = core::ffi::CStr::from_ptr(info.dli_fname)
                .to_str()
                .unwrap_or("?");
            eprintln!("  {:32} → {:p}  ({})", sym_name, p, fname);
        } else {
            eprintln!("  {:32} → {:p}  (dladdr failed)", sym_name, p);
        }
    }
}

// ── RTLD_NEXT fallback — lock-free per-symbol slots ───────────────────────────
//
// We store each RTLD_NEXT function pointer in a plain AtomicUsize (0 = not yet
// resolved).  On first use, dlsym(RTLD_NEXT, name) is called and the result is
// stored.  No OnceLock / mutex is used, which avoids a deadlock where dlsym
// itself calls pthread_mutex_lock → our interceptor → this initialiser → dlsym
// (OnceLock in RUNNING state → single-thread futex deadlock).

static NEXT_PTHREAD_CREATE: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_JOIN: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_LOCK: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_TRYLOCK: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_UNLOCK: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_INIT: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEX_DESTROY: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEXATTR_INIT: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEXATTR_SETTYPE: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_MUTEXATTR_DESTROY: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_COND_WAIT: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_COND_SIGNAL: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_COND_BROADCAST: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_BARRIER_INIT: AtomicUsize = AtomicUsize::new(0);
static NEXT_PTHREAD_BARRIER_WAIT: AtomicUsize = AtomicUsize::new(0);
static NEXT_SCHED_YIELD: AtomicUsize = AtomicUsize::new(0);
static NEXT_NANOSLEEP: AtomicUsize = AtomicUsize::new(0);
static NEXT_USLEEP: AtomicUsize = AtomicUsize::new(0);
static NEXT_SLEEP: AtomicUsize = AtomicUsize::new(0);
static NEXT_CLOCK_NANOSLEEP: AtomicUsize = AtomicUsize::new(0);
static NEXT_FORK: AtomicUsize = AtomicUsize::new(0);
static NEXT_EXECV: AtomicUsize = AtomicUsize::new(0);
static NEXT_EXECVE: AtomicUsize = AtomicUsize::new(0);
static NEXT_WAITPID: AtomicUsize = AtomicUsize::new(0);
static NEXT_SYSCALL: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "tsan")]
static TSAN_PTHREAD_CREATE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "tsan")]
static TSAN_BACKGROUND_MODE: AtomicUsize = AtomicUsize::new(usize::MAX);

#[cfg(feature = "tsan")]
const TSAN_BG_FIRST: usize = 1 << 0;
#[cfg(feature = "tsan")]
const TSAN_BG_RETURN_ADDRESS: usize = 1 << 1;
#[cfg(feature = "tsan")]
const TSAN_BG_START_ARG: usize = 1 << 2;

/// Load (and lazily resolve) a single RTLD_NEXT function pointer.
/// Safe to call re-entrantly on a single thread: worst case two threads both
/// call dlsym and the second store overwrites the first with the same value.
#[inline]
unsafe fn load_next<T: Copy>(slot: &AtomicUsize, name: &[u8]) -> T {
    let mut p = slot.load(Ordering::Acquire);
    if p == 0 {
        p = libc::dlsym(RTLD_NEXT, name.as_ptr() as *const _) as usize;
        assert!(
            p != 0,
            "rsched preload: dlsym(RTLD_NEXT, {:?}) returned null",
            core::str::from_utf8(name).unwrap_or("?")
        );
        slot.store(p, Ordering::Release);
    }
    std::mem::transmute_copy::<usize, T>(&p)
}

#[cfg(feature = "tsan")]
unsafe extern "C" {
    fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
}

#[cfg(feature = "tsan")]
#[inline(never)]
fn tsan_background_mode() -> usize {
    let cached = TSAN_BACKGROUND_MODE.load(Ordering::Acquire);
    if cached != usize::MAX {
        return cached;
    }

    let raw = std::env::var("RSCHED_TSAN_BACKGROUND_THREAD").unwrap_or_else(|_| "first".into());
    let mode = parse_tsan_background_mode(&raw);
    TSAN_BACKGROUND_MODE.store(mode, Ordering::Release);
    mode
}

#[cfg(feature = "tsan")]
fn parse_tsan_background_mode(raw: &str) -> usize {
    let mut mode = 0;
    for token in
        raw.split(|c: char| c == '&' || c == ',' || c == '+' || c == ':' || c.is_whitespace())
    {
        if token.is_empty() {
            continue;
        }

        match token {
            "none" => {
                if mode != 0 {
                    panic!(
                        "rsched preload: RSCHED_TSAN_BACKGROUND_THREAD=none cannot be combined with other criteria"
                    );
                }
                return 0;
            }
            "first" | "first-thread" => mode |= TSAN_BG_FIRST,
            "return-address" | "return_address" | "return" | "ra" => mode |= TSAN_BG_RETURN_ADDRESS,
            "start-arg" | "start_arg" | "start" | "bytes" => mode |= TSAN_BG_START_ARG,
            other => panic!(
                "rsched preload: unknown RSCHED_TSAN_BACKGROUND_THREAD criterion {:?}",
                other
            ),
        }
    }

    if mode == 0 { TSAN_BG_FIRST } else { mode }
}

#[cfg(feature = "tsan")]
fn read_u16(data: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(off..off + 2)?.try_into().ok()?))
}

#[cfg(feature = "tsan")]
fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
}

#[cfg(feature = "tsan")]
fn read_u64(data: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(off..off + 8)?.try_into().ok()?))
}

#[cfg(feature = "tsan")]
fn cstr_at(data: &[u8], off: usize) -> Option<&[u8]> {
    let tail = data.get(off..)?;
    let end = tail.iter().position(|&b| b == 0)?;
    Some(&tail[..end])
}

#[cfg(feature = "tsan")]
fn elf_symbol_contains(path: &str, offset: usize, expected: &[u8]) -> bool {
    const SHT_SYMTAB: u32 = 2;
    const STT_FUNC: u8 = 2;

    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(_) => return false,
    };
    if data.get(0..4) != Some(b"\x7fELF") || data.get(4) != Some(&2) || data.get(5) != Some(&1) {
        return false;
    }

    let shoff = match read_u64(&data, 0x28) {
        Some(v) => v as usize,
        None => return false,
    };
    let shentsize = match read_u16(&data, 0x3a) {
        Some(v) => v as usize,
        None => return false,
    };
    let shnum = match read_u16(&data, 0x3c) {
        Some(v) => v as usize,
        None => return false,
    };
    if shentsize < 64 {
        return false;
    }

    for idx in 0..shnum {
        let sh = shoff + idx * shentsize;
        let sh_type = match read_u32(&data, sh + 4) {
            Some(v) => v,
            None => return false,
        };
        if sh_type != SHT_SYMTAB {
            continue;
        }

        let sym_off = match read_u64(&data, sh + 0x18) {
            Some(v) => v as usize,
            None => return false,
        };
        let sym_size = match read_u64(&data, sh + 0x20) {
            Some(v) => v as usize,
            None => return false,
        };
        let sym_entsize = match read_u64(&data, sh + 0x38) {
            Some(v) => v as usize,
            None => return false,
        };
        let str_idx = match read_u32(&data, sh + 0x28) {
            Some(v) => v as usize,
            None => return false,
        };
        if sym_entsize < 24 || str_idx >= shnum {
            return false;
        }

        let str_sh = shoff + str_idx * shentsize;
        let str_off = match read_u64(&data, str_sh + 0x18) {
            Some(v) => v as usize,
            None => return false,
        };
        let str_size = match read_u64(&data, str_sh + 0x20) {
            Some(v) => v as usize,
            None => return false,
        };
        let strtab = match data.get(str_off..str_off + str_size) {
            Some(v) => v,
            None => return false,
        };

        let sym_count = sym_size / sym_entsize;
        for sym_idx in 0..sym_count {
            let sym = sym_off + sym_idx * sym_entsize;
            let name_off = match read_u32(&data, sym) {
                Some(v) => v as usize,
                None => return false,
            };
            let info = match data.get(sym + 4) {
                Some(v) => *v,
                None => return false,
            };
            if info & 0x0f != STT_FUNC {
                continue;
            }

            let value = match read_u64(&data, sym + 8) {
                Some(v) => v as usize,
                None => return false,
            };
            let size = match read_u64(&data, sym + 16) {
                Some(v) => v as usize,
                None => return false,
            };
            if size == 0 || offset < value || offset >= value + size {
                continue;
            }

            return cstr_at(strtab, name_off) == Some(expected);
        }
    }

    false
}

#[cfg(feature = "tsan")]
#[inline(never)]
unsafe fn caller_is_tsan_internal_start_thread() -> bool {
    let mut frames: [*mut c_void; 12] = [core::ptr::null_mut(); 12];
    let n = backtrace(frames.as_mut_ptr(), frames.len() as c_int);
    if n <= 2 {
        return false;
    }

    for caller in frames.iter().take(n as usize).skip(2) {
        if caller_is_symbol(
            *caller as usize,
            b"_ZN11__sanitizer21internal_start_threadEPFPvS0_ES0_",
        ) {
            return true;
        }
    }

    false
}

#[cfg(feature = "tsan")]
unsafe fn caller_is_symbol(caller: usize, expected: &[u8]) -> bool {
    let mut info: libc::Dl_info = core::mem::zeroed();
    if libc::dladdr(caller as *const c_void, &mut info) == 0 || info.dli_fname.is_null() {
        return false;
    }

    let file = match core::ffi::CStr::from_ptr(info.dli_fname).to_str() {
        Ok(file) => file,
        Err(_) => return false,
    };
    let offset = caller.saturating_sub(info.dli_fbase as usize);
    elf_symbol_contains(file, offset, expected)
}

#[cfg(feature = "tsan")]
unsafe fn is_tsan_background_thread_create(
    start: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> bool {
    let mode = tsan_background_mode();
    if mode == 0 {
        return false;
    }

    let ordinal = TSAN_PTHREAD_CREATE_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    let first_match = ordinal == 1;
    let return_match = caller_is_tsan_internal_start_thread();
    let start_arg_match = rsched_is_tsan_background_start(start, arg);

    let mut any_selected_match = false;
    let mut all_selected_match = true;
    for (bit, matched) in [
        (TSAN_BG_FIRST, first_match),
        (TSAN_BG_RETURN_ADDRESS, return_match),
        (TSAN_BG_START_ARG, start_arg_match),
    ] {
        if mode & bit == 0 {
            continue;
        }
        any_selected_match |= matched;
        all_selected_match &= matched;
    }

    if any_selected_match && !all_selected_match {
        panic!(
            "rsched preload: partial TSAN background thread match: mode={:?}, pthread_create_ordinal={}, first={}, return_address={}, start_arg={}",
            std::env::var("RSCHED_TSAN_BACKGROUND_THREAD").unwrap_or_else(|_| "first".into()),
            ordinal,
            first_match,
            return_match,
            start_arg_match,
        );
    }

    all_selected_match
}

// ── Thread lifecycle ──────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    attr: *const pthread_attr_t,
    start: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(
            *mut pthread_t,
            *const pthread_attr_t,
            unsafe extern "C" fn(*mut c_void) -> *mut c_void,
            *mut c_void,
        ) -> c_int = load_next(&NEXT_PTHREAD_CREATE, b"pthread_create\0");
        return f(thread, attr, start, arg);
    }
    #[cfg(feature = "tsan")]
    if is_tsan_background_thread_create(start, arg) {
        let f: unsafe extern "C" fn(
            *mut pthread_t,
            *const pthread_attr_t,
            unsafe extern "C" fn(*mut c_void) -> *mut c_void,
            *mut c_void,
        ) -> c_int = load_next(&NEXT_PTHREAD_CREATE, b"pthread_create\0");
        return f(thread, attr, start, arg);
    }
    #[cfg(feature = "tsan")]
    if !rsched_is_tsan_thread_start(start, arg) {
        let f: unsafe extern "C" fn(
            *mut pthread_t,
            *const pthread_attr_t,
            unsafe extern "C" fn(*mut c_void) -> *mut c_void,
            *mut c_void,
        ) -> c_int = load_next(&NEXT_PTHREAD_CREATE, b"pthread_create\0");
        return f(thread, attr, start, arg);
    }
    rsched_pthread_create(thread, attr, start, arg)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(pthread_t, *mut *mut c_void) -> c_int =
            load_next(&NEXT_PTHREAD_JOIN, b"pthread_join\0");
        return f(thread, retval);
    }
    rsched_pthread_join(thread, retval)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_exit(retval: *mut c_void) -> ! {
    // diverging: no defer!(exit()) — always route through rsched which never
    // returns, so there is no exit to balance.
    rsched_try_enter();
    rsched_pthread_exit(retval)
}

// ── Mutex ─────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_init(attr: *mut pthread_mutexattr_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    let f: unsafe extern "C" fn(*mut pthread_mutexattr_t) -> c_int =
        load_next(&NEXT_PTHREAD_MUTEXATTR_INIT, b"pthread_mutexattr_init\0");
    let r = f(attr);
    if outermost && r == 0 {
        rsched_note_pthread_mutexattr_init(attr);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_settype(
    attr: *mut pthread_mutexattr_t,
    kind: c_int,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    let f: unsafe extern "C" fn(*mut pthread_mutexattr_t, c_int) -> c_int = load_next(
        &NEXT_PTHREAD_MUTEXATTR_SETTYPE,
        b"pthread_mutexattr_settype\0",
    );
    let r = f(attr, kind);
    if outermost && r == 0 {
        rsched_note_pthread_mutexattr_settype(attr, kind);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_destroy(attr: *mut pthread_mutexattr_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    let f: unsafe extern "C" fn(*mut pthread_mutexattr_t) -> c_int = load_next(
        &NEXT_PTHREAD_MUTEXATTR_DESTROY,
        b"pthread_mutexattr_destroy\0",
    );
    let r = f(attr);
    if outermost && r == 0 {
        rsched_note_pthread_mutexattr_destroy(attr);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_init(
    m: *mut pthread_mutex_t,
    attr: *const pthread_mutexattr_t,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    let f: unsafe extern "C" fn(*mut pthread_mutex_t, *const pthread_mutexattr_t) -> c_int =
        load_next(&NEXT_PTHREAD_MUTEX_INIT, b"pthread_mutex_init\0");
    let r = f(m, attr);
    if outermost && r == 0 {
        rsched_note_pthread_mutex_init(m, attr);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_destroy(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
        load_next(&NEXT_PTHREAD_MUTEX_DESTROY, b"pthread_mutex_destroy\0");
    let r = f(m);
    if outermost && r == 0 {
        rsched_note_pthread_mutex_destroy(m);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_lock(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_MUTEX_LOCK, b"pthread_mutex_lock\0");
        return f(m);
    }
    rsched_pthread_mutex_lock(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_trylock(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_MUTEX_TRYLOCK, b"pthread_mutex_trylock\0");
        return f(m);
    }
    rsched_pthread_mutex_trylock(m)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut pthread_mutex_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_MUTEX_UNLOCK, b"pthread_mutex_unlock\0");
        return f(m);
    }
    rsched_pthread_mutex_unlock(m)
}

// ── Condition variables ───────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_cond_t, *mut pthread_mutex_t) -> c_int =
            load_next(&NEXT_PTHREAD_COND_WAIT, b"pthread_cond_wait\0");
        return f(cond, mutex);
    }
    rsched_pthread_cond_wait(cond, mutex)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_cond_t) -> c_int =
            load_next(&NEXT_PTHREAD_COND_SIGNAL, b"pthread_cond_signal\0");
        return f(cond);
    }
    rsched_pthread_cond_signal(cond)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_cond_t) -> c_int =
            load_next(&NEXT_PTHREAD_COND_BROADCAST, b"pthread_cond_broadcast\0");
        return f(cond);
    }
    rsched_pthread_cond_broadcast(cond)
}

// ── Barriers ──────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_barrier_init(
    barrier: *mut pthread_barrier_t,
    attr: *const pthread_barrierattr_t,
    count: c_uint,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(
            *mut pthread_barrier_t,
            *const pthread_barrierattr_t,
            c_uint,
        ) -> c_int = load_next(&NEXT_PTHREAD_BARRIER_INIT, b"pthread_barrier_init\0");
        return f(barrier, attr, count);
    }
    rsched_pthread_barrier_init(barrier, attr, count)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_barrier_wait(barrier: *mut pthread_barrier_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*mut pthread_barrier_t) -> c_int =
            load_next(&NEXT_PTHREAD_BARRIER_WAIT, b"pthread_barrier_wait\0");
        return f(barrier);
    }
    rsched_pthread_barrier_wait(barrier)
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_yield() -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn() -> c_int = load_next(&NEXT_SCHED_YIELD, b"sched_yield\0");
        return f();
    }
    rsched_sched_yield()
}

// ── Processes ────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fork() -> libc::pid_t {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    let f: unsafe extern "C" fn() -> libc::pid_t = load_next(&NEXT_FORK, b"fork\0");
    if !outermost {
        return f();
    }

    rsched_before_fork();
    let pid = f();
    if pid == 0 {
        rsched_after_fork_child();
    } else {
        rsched_after_fork_parent(pid);
    }
    pid
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall(
    number: libc::c_long,
    a0: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
) -> libc::c_long {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        return rsched_raw_syscall(number, a0, a1, a2, a3, a4, a5);
    }
    if number == libc::SYS_clone {
        return rsched_syscall(number, a0, a1, a2, a3, a4, a5);
    }

    let f: unsafe extern "C" fn(
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
    ) -> libc::c_long = load_next(&NEXT_SYSCALL, b"syscall\0");
    f(number, a0, a1, a2, a3, a4, a5)
}

unsafe fn exit_process(status: c_int) -> ! {
    rsched_process_exit();
    rsched_raw_syscall(libc::SYS_exit_group, status.into(), 0, 0, 0, 0, 0);
    core::hint::unreachable_unchecked()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _exit(status: c_int) -> ! {
    exit_process(status)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Exit(status: c_int) -> ! {
    exit_process(status)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execv(
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(*const libc::c_char, *const *const libc::c_char) -> c_int =
            load_next(&NEXT_EXECV, b"execv\0");
        return f(path, argv);
    }
    rsched_execv(path, argv)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execve(
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(
            *const libc::c_char,
            *const *const libc::c_char,
            *const *const libc::c_char,
        ) -> c_int = load_next(&NEXT_EXECVE, b"execve\0");
        return f(path, argv, envp);
    }
    rsched_execve(path, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn waitpid(
    pid: libc::pid_t,
    status: *mut libc::c_int,
    options: c_int,
) -> libc::pid_t {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if !outermost {
        let f: unsafe extern "C" fn(libc::pid_t, *mut libc::c_int, c_int) -> libc::pid_t =
            load_next(&NEXT_WAITPID, b"waitpid\0");
        return f(pid, status, options);
    }
    rsched_waitpid(pid, status, options)
}

// ── Blocking sleeps ───────────────────────────────────────────────────────────

unsafe fn virtual_sleep_yields(mut n: u64) {
    n = n.saturating_mul(100).clamp(1, 100_000);
    for _ in 0..n {
        rsched_sched_yield();
    }
}

fn timespec_yields(req: *const timespec) -> u64 {
    if req.is_null() {
        return 1;
    }
    unsafe {
        let sec = (*req).tv_sec.max(0) as u64;
        let nsec = (*req).tv_nsec.max(0) as u64;
        sec.saturating_mul(1000)
            .saturating_add(nsec.saturating_add(999_999) / 1_000_000)
            .max(1)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nanosleep(req: *const timespec, rem: *mut timespec) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if outermost {
        let _ = rem;
        virtual_sleep_yields(timespec_yields(req));
        return 0;
    }
    let f: unsafe extern "C" fn(*const timespec, *mut timespec) -> c_int =
        load_next(&NEXT_NANOSLEEP, b"nanosleep\0");
    f(req, rem)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_nanosleep(
    clockid: libc::clockid_t,
    flags: c_int,
    req: *const timespec,
    rem: *mut timespec,
) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if outermost {
        let _ = clockid;
        let _ = flags;
        let _ = rem;
        virtual_sleep_yields(timespec_yields(req));
        return 0;
    }
    let f: unsafe extern "C" fn(libc::clockid_t, c_int, *const timespec, *mut timespec) -> c_int =
        load_next(&NEXT_CLOCK_NANOSLEEP, b"clock_nanosleep\0");
    f(clockid, flags, req, rem)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn usleep(usec: libc::useconds_t) -> c_int {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if outermost {
        virtual_sleep_yields((usec as u64).saturating_add(999) / 1000);
        return 0;
    }
    let f: unsafe extern "C" fn(libc::useconds_t) -> c_int = load_next(&NEXT_USLEEP, b"usleep\0");
    f(usec)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sleep(secs: c_uint) -> c_uint {
    let outermost = rsched_try_enter();
    defer!(rsched_exit());
    if outermost {
        virtual_sleep_yields((secs as u64).saturating_mul(1000));
        return 0;
    }
    let f: unsafe extern "C" fn(c_uint) -> c_uint = load_next(&NEXT_SLEEP, b"sleep\0");
    f(secs)
}
