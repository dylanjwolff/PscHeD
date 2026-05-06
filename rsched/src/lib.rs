//! rsched – Random-walk cooperative thread scheduler, C-compatible library.
//!
//! Exports `rsched_pthread_*` / `rsched_sched_yield` symbols.
//! A companion C header (`include/rsched.h`) maps those over the standard
//! pthread names so that C programs can link against this library without
//! any LD_PRELOAD trickery.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(internal_features)]
#![feature(link_llvm_intrinsics)]

use std::collections::HashMap;
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::cell::RefCell;

mod scheduler;
use scheduler::{Scheduler, RandomWalk, LoggingScheduler};

mod event;
pub use event::{AccessKind, Event, EventKind};

// ── Type aliases ──────────────────────────────────────────────────────────

type PthreadT  = libc::pthread_t;
type MutexT    = libc::pthread_mutex_t;
type CondT     = libc::pthread_cond_t;
type BarrierT  = libc::pthread_barrier_t;
type AttrT     = libc::pthread_attr_t;

// ── Thread descriptor ─────────────────────────────────────────────────────

struct Thread {
    tid: libc::pid_t,
    pthread: PthreadT,
    is_blocking: bool,
    /// False until the parent's rsched_pthread_create returns and the sanitizer's
    /// PostCreate (or equivalent) has been called for this thread.  While false,
    /// context_switch never wakes this thread, preventing it from running
    /// asan_thread_start / tsan_thread_start before the parent has finished the
    /// thread-creation handshake.  The main thread is born with startup_done=true.
    startup_done: bool,
    /// Condition the scheduler uses to suspend/resume this thread.
    /// All suspensions wait on this cond with GMTX held.
    suspend_cond: CondT,
    /// Thread waiting on this one via pthread_join.
    joiner: Option<PthreadT>,
    /// What this thread is about to do at the next scheduling point.
    /// Set before each context switch, cleared after the operation completes.
    pub next_event: Option<Event>,
}

impl Thread {
    fn new(pt: PthreadT) -> Self {
        Thread {
            tid: unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t },
            pthread: pt,
            is_blocking: false,
            startup_done: false,
            suspend_cond: libc::PTHREAD_COND_INITIALIZER,
            joiner: None,
            next_event: None,
        }
    }
}

// ── Mutex / Cond / Barrier state ──────────────────────────────────────────

struct SMutex {
    owner_tid: libc::pid_t, // 0 = unlocked
    recursive_count: u32,
    waiters: Vec<PthreadT>,
}

struct SCond {
    waiters: Vec<PthreadT>,
}

struct SBarrier {
    count: u32,
    waiters: Vec<PthreadT>,
}

// ── Scheduler state ───────────────────────────────────────────────────────

struct State {
    scheduler: Box<dyn Scheduler>,
    threads:   Vec<PthreadT>,
    info:     HashMap<PthreadT, Thread>,
    mutexes:  HashMap<usize, SMutex>,
    conds:    HashMap<usize, SCond>,
    barriers: HashMap<usize, SBarrier>,
}

impl State {
    fn new(seed: u64) -> Self {
        let logging = std::env::var("RSCHED_LOG").map_or(false, |v| v == "1");
        let scheduler: Box<dyn Scheduler> = if logging {
            Box::new(LoggingScheduler::new(RandomWalk::new(seed)))
        } else {
            Box::new(RandomWalk::new(seed))
        };
        State {
            scheduler,
            threads:   Vec::new(),
            info:      HashMap::new(),
            mutexes:   HashMap::new(),
            conds:     HashMap::new(),
            barriers:  HashMap::new(),
        }
    }

    fn add(&mut self, t: Thread) {
        let pt = t.pthread;
        self.threads.push(pt);
        self.info.insert(pt, t);
    }

    fn remove(&mut self, pt: PthreadT) {
        self.threads.retain(|&x| x != pt);
        self.info.remove(&pt);
    }

    fn t(&mut self, pt: PthreadT) -> &mut Thread {
        self.info.get_mut(&pt).expect("rsched: unknown thread")
    }

    fn choose(&mut self) -> Option<usize> {
        let blocking: Vec<bool> = self.threads.iter()
            .map(|pt| {
                let t = &self.info[pt];
                t.is_blocking || !t.startup_done
            })
            .collect();
        self.scheduler.choose(&blocking)
    }

    /// Signal `next`'s suspend_cond and, if `suspend_caller` and next≠caller,
    /// suspend `caller` by waiting on its own cond (releases GMTX atomically).
    /// Must be called with GMTX held.
    unsafe fn wake(&mut self, next: PthreadT, suspend_caller: bool, caller: PthreadT) {
        if next != caller {
            let cond_ptr = addr_of_mut!(self.info.get_mut(&next).unwrap().suspend_cond);
            (rpt().cond_signal)(cond_ptr);
            if suspend_caller {
                let my_cond = addr_of_mut!(self.info.get_mut(&caller).unwrap().suspend_cond);
                (rpt().cond_wait)(my_cond, addr_of_mut!(GMTX));
            }
        }
    }

    /// Pick a random non-blocking thread and hand control to it, suspending caller.
    unsafe fn context_switch(&mut self, caller: PthreadT) {
        if self.threads.len() <= 1 { return; }
        let idx = match self.choose() {
            Some(i) => i,
            None => { eprintln!("[rsched] deadlock"); libc::abort(); }
        };
        let next = self.threads[idx];
        self.wake(next, true, caller);
        let event = self.info.get(&caller).and_then(|t| t.next_event);
        self.scheduler.on_event(event.as_ref());
    }

    // ── event helpers ─────────────────────────────────────────────────

    fn set_event(&mut self, pt: PthreadT, ev: Event) {
        self.t(pt).next_event = Some(ev);
    }

    fn clear_event(&mut self, pt: PthreadT) {
        self.t(pt).next_event = None;
    }

    // ── mutex helpers ─────────────────────────────────────────────────

    fn will_block(&self, key: usize, tid: libc::pid_t) -> bool {
        match self.mutexes.get(&key) {
            Some(m) => m.owner_tid != 0 && m.owner_tid != tid,
            None => false,
        }
    }

    unsafe fn mutex_lock(&mut self, key: usize, caller: PthreadT) {
        let tid = self.info[&caller].tid;
        loop {
            let e = self.mutexes.entry(key).or_insert(SMutex {
                owner_tid: tid, recursive_count: 0, waiters: Vec::new(),
            });
            if e.owner_tid == tid { e.recursive_count += 1; return; }
            if !e.waiters.contains(&caller) { e.waiters.push(caller); }
            self.t(caller).is_blocking = true;
            self.context_switch(caller);
        }
    }

    unsafe fn mutex_unlock(&mut self, key: usize, caller: PthreadT) -> libc::c_int {
        let tid = self.info[&caller].tid;
        // Phase 1: validate and decrement. Produce waiter_count; borrow ends at block exit.
        let waiter_count = {
            let m = match self.mutexes.get_mut(&key) {
                Some(m) => m,
                None => return libc::EINVAL,
            };
            if m.owner_tid != tid { return libc::EPERM; }
            m.recursive_count -= 1;
            if m.recursive_count > 0 { return 0; }
            m.waiters.len()
        }; // borrow of self.mutexes released here
        if waiter_count == 0 { self.mutexes.remove(&key); return 0; }
        // Phase 2: pick a random waiter (needs self.scheduler, no mutexes borrow active).
        let blocking = vec![false; waiter_count];
        let idx = self.scheduler.choose(&blocking).expect("rsched: non-empty waiter list");
        // Phase 3: transfer ownership to chosen waiter.
        let waiter = self.mutexes.get_mut(&key).unwrap().waiters.remove(idx);
        let waiter_tid = self.info[&waiter].tid;
        self.mutexes.get_mut(&key).unwrap().owner_tid = waiter_tid;
        self.t(waiter).is_blocking = false;
        0
    }
}

// ── Globals ───────────────────────────────────────────────────────────────

static mut GMTX:  MutexT       = libc::PTHREAD_MUTEX_INITIALIZER;
static mut STATE: Option<State> = None;
static INITED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static MY_PT: RefCell<PthreadT> = const { RefCell::new(0) };
}

fn my_pt() -> PthreadT { MY_PT.with(|c| *c.borrow()) }

// ── Return-address intrinsic ──────────────────────────────────────────────

unsafe extern "C" {
    /// LLVM intrinsic: address in the caller that the current function will
    /// return to.  Level 0 = immediate caller; equivalent to
    /// `__builtin_return_address(0)` in GCC/Clang or `@returnAddress()` in Zig.
    #[link_name = "llvm.returnaddress"]
    fn llvm_returnaddress(level: i32) -> *const u8;
}

/// Returns the address in the caller's code that triggered the current
/// scheduling point.  Must be `#[inline(always)]` so the captured address
/// belongs to the outermost exported function frame, not an rsched helper.
#[inline(always)]
fn return_address() -> u64 {
    unsafe { llvm_returnaddress(0) as u64 }
}

// ── Direct libpthread bindings (bypass PLT / TSAN / preload shim) ─────────
//
// rsched uses its own internal synchronisation primitives (GMTX, suspend_cond,
// ready_cond) that must NOT go through the preload shim.  When the preload
// cdylib exports `pthread_mutex_lock`, rsched's ordinary PLT calls route:
//
//   rsched glock() → PLT → TSAN interceptor → preload pthread_mutex_lock
//                                              → rsched_pthread_mutex_lock
//                                              → glock() …  (deadlock / depth-2 panic)
//
// Fetching the real libpthread symbols via dlopen/dlsym breaks the cycle:
// those raw function-pointer calls bypass both the PLT and the TSAN
// interceptors, so rsched's internal calls reach libpthread directly.

#[allow(dead_code)] // `create` unused when --features asan routes through PLT
struct RealPt {
    mutex_lock:   unsafe extern "C" fn(*mut MutexT) -> libc::c_int,
    mutex_unlock: unsafe extern "C" fn(*mut MutexT) -> libc::c_int,
    cond_wait:    unsafe extern "C" fn(*mut CondT, *mut MutexT) -> libc::c_int,
    cond_signal:  unsafe extern "C" fn(*mut CondT) -> libc::c_int,
    create: unsafe extern "C" fn(
        *mut PthreadT, *const AttrT,
        unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void,
        *mut libc::c_void,
    ) -> libc::c_int,
    join:         unsafe extern "C" fn(PthreadT, *mut *mut libc::c_void) -> libc::c_int,
    barrier_init: unsafe extern "C" fn(
        *mut BarrierT,
        *const libc::pthread_barrierattr_t,
        libc::c_uint,
    ) -> libc::c_int,
}

static REAL_PT: std::sync::OnceLock<RealPt> = std::sync::OnceLock::new();

unsafe fn rpt() -> &'static RealPt {
    REAL_PT.get_or_init(|| {
        // Try the traditional stub first; on glibc >= 2.34 the symbols live in
        // libc.so.6 and libpthread.so.0 is a forwarding stub that still responds
        // to dlsym correctly.  RTLD_NOLOAD avoids loading anything new.
        let mut lib = libc::dlopen(
            b"libpthread.so.0\0".as_ptr() as *const _,
            libc::RTLD_LAZY | libc::RTLD_NOLOAD,
        );
        if lib.is_null() {
            lib = libc::dlopen(
                b"libc.so.6\0".as_ptr() as *const _,
                libc::RTLD_LAZY | libc::RTLD_NOLOAD,
            );
        }
        assert!(!lib.is_null(), "rsched: cannot resolve libpthread/libc via dlopen");

        // Helper: look up one symbol and transmute to the target function-pointer
        // type.  Function pointers and data pointers share the same width on every
        // platform rsched targets, so the transmute is sound.
        unsafe fn sym<T: Copy>(lib: *mut libc::c_void, name: &[u8]) -> T {
            let p = libc::dlsym(lib, name.as_ptr() as *const _);
            assert!(!p.is_null(), "rsched: dlsym returned null");
            std::mem::transmute_copy::<*mut libc::c_void, T>(&p)
        }

        RealPt {
            mutex_lock:   sym(lib, b"pthread_mutex_lock\0"),
            mutex_unlock: sym(lib, b"pthread_mutex_unlock\0"),
            cond_wait:    sym(lib, b"pthread_cond_wait\0"),
            cond_signal:  sym(lib, b"pthread_cond_signal\0"),
            create:       sym(lib, b"pthread_create\0"),
            join:         sym(lib, b"pthread_join\0"),
            barrier_init: sym(lib, b"pthread_barrier_init\0"),
        }
    })
}

unsafe fn glock()   { (rpt().mutex_lock)(addr_of_mut!(GMTX)); }
unsafe fn gunlock() { (rpt().mutex_unlock)(addr_of_mut!(GMTX)); }

// ── Reentrancy depth tracking ─────────────────────────────────────────────────
//
// CALL_DEPTH lives here so that both the preload interceptors and rsched's own
// trampoline/do_thread_exit share a single counter per thread.  The preload
// cdylib links rsched as an rlib, so all code ends up in the same DSO and
// shares this TLS slot.
//
// trampoline() and do_thread_exit() run in freshly created OS threads where
// CALL_DEPTH=0.  They call depth_enter() before any rpt() primitive so that
// any libc-internal PLT re-entry through the preload interceptors is treated as
// non-outermost and forwarded to RTLD_NEXT instead of looping back into rsched
// (which would deadlock on GMTX).

thread_local! {
    static CALL_DEPTH: AtomicU32 = const { AtomicU32::new(0) };
}

/// Increment depth; return true iff this is the outermost (non-reentrant) call.
/// Called by the preload interceptors on every entry.
pub fn rsched_try_enter() -> bool {
    CALL_DEPTH.with(|d| d.fetch_add(1, Ordering::Acquire) == 0)
}

/// Decrement depth. Called by the preload interceptors on every exit.
pub fn rsched_exit() {
    CALL_DEPTH.with(|d| { d.fetch_sub(1, Ordering::Release); });
}

/// Increment depth without checking. Used internally by trampoline/do_thread_exit.
#[inline]
fn depth_enter() {
    CALL_DEPTH.with(|d| { d.fetch_add(1, Ordering::Acquire); });
}

/// Decrement depth. Paired with depth_enter().
#[inline]
fn depth_exit() {
    CALL_DEPTH.with(|d| { d.fetch_sub(1, Ordering::Release); });
}

// ── TSAN happens-before annotations ──────────────────────────────────────────
//
// When the preload cdylib is built with `--features tsan`, these wrappers call
// into the ThreadSanitizer runtime to record the acquire/release edges that
// rsched's virtual mutexes create.  Without them TSAN cannot see the
// happens-before established by rsched's scheduler and would report false
// positives on correctly-synchronised programs.
//
// The symbols are resolved at load time from the TSAN runtime that the
// -fsanitize=thread program carries; the cdylib itself does not link against
// libtsan.  Building *without* the feature leaves no-op stubs so the rest of
// the code is identical in both configurations.

#[cfg(feature = "tsan")]
unsafe fn tsan_acquire(addr: *mut libc::c_void) {
    unsafe extern "C" {
        fn __tsan_acquire(addr: *mut libc::c_void);
    }
    __tsan_acquire(addr);
}

#[cfg(not(feature = "tsan"))]
#[inline(always)]
unsafe fn tsan_acquire(_addr: *mut libc::c_void) {}

#[cfg(feature = "tsan")]
unsafe fn tsan_release(addr: *mut libc::c_void) {
    unsafe extern "C" {
        fn __tsan_release(addr: *mut libc::c_void);
    }
    __tsan_release(addr);
}

#[cfg(not(feature = "tsan"))]
#[inline(always)]
unsafe fn tsan_release(_addr: *mut libc::c_void) {}

unsafe fn st()      -> &'static mut State {
    (*addr_of_mut!(STATE)).as_mut().expect("rsched not initialised")
}

// ── rsched_init ───────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_init() {
    if INITED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return;
    }
    let seed: u64 = std::env::var("RANDOM_SEED")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(0x12345678abcdu64);
    STATE = Some(State::new(seed));

    let self_pt = libc::pthread_self();
    MY_PT.with(|c| *c.borrow_mut() = self_pt);
    glock();
    let mut t = Thread::new(self_pt);
    t.startup_done = true;  // main thread needs no sanitizer handshake
    st().add(t);
    gunlock();
}

fn ensure_init() {
    if !INITED.load(Ordering::SeqCst) { unsafe { rsched_init(); } }
}

// ── rsched_reinit ─────────────────────────────────────────────────────────

/// Reset the scheduler with a new seed.  Must be called from the main thread
/// only, after all previously-spawned threads have been joined.
/// Used by integration tests to run multiple scenarios in one process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_reinit(seed: u64) {
    // Drop any existing state (previous test run's threads/mutexes/etc.).
    STATE = None;
    STATE = Some(State::new(seed));
    INITED.store(true, Ordering::SeqCst);

    let self_pt = libc::pthread_self();
    MY_PT.with(|c| *c.borrow_mut() = self_pt);
    glock();
    let mut t = Thread::new(self_pt);
    t.startup_done = true;
    st().add(t);
    gunlock();
}

// ── Thread trampoline ─────────────────────────────────────────────────────

struct StartArg {
    routine:    unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void,
    arg:        *mut libc::c_void,
    /// Signaled (with GMTX held) once the new thread has registered itself.
    ready_cond: CondT,
    ready:      bool,
}
unsafe impl Send for StartArg {}

/// Shared cleanup logic for thread exit.  Must be called with GMTX *not* held.
unsafe fn do_thread_exit(caller: PthreadT) {
    // Raise CALL_DEPTH before acquiring GMTX so that any libc-internal PLT
    // re-entry through our preload interceptors is treated as non-outermost.
    depth_enter();
    glock();
    let joiner_opt = st().info.get(&caller).and_then(|t| t.joiner);
    if let Some(joiner) = joiner_opt {
        st().t(joiner).is_blocking = false;
    }
    st().remove(caller);
    if !st().threads.is_empty() {
        if let Some(idx) = st().choose() {
            let next = st().threads[idx];
            let cond_ptr = addr_of_mut!(st().t(next).suspend_cond);
            (rpt().cond_signal)(cond_ptr);
        }
    }
    gunlock();
    depth_exit();
}

// Safe fn required because libc::pthread_create takes a safe fn pointer.
extern "C" fn trampoline(raw: *mut libc::c_void) -> *mut libc::c_void {
    unsafe {
        let sa = &mut *(raw as *mut StartArg);
        let routine = sa.routine;
        let arg     = sa.arg;

        let self_pt = libc::pthread_self();
        MY_PT.with(|c| *c.borrow_mut() = self_pt);

        // Raise CALL_DEPTH before calling any rpt() primitive so that any
        // libc-internal PLT re-entry (e.g. cond_wait releasing GMTX via
        // pthread_mutex_unlock through our preload's interceptor) is seen as
        // non-outermost and forwarded to RTLD_NEXT instead of routing back
        // into rsched (which would try to re-acquire GMTX → deadlock).
        depth_enter();

        // Acquire GMTX (creator released it via pthread_cond_wait below).
        glock();
        st().add(Thread::new(self_pt));

        // Signal creator that we are registered; it will re-acquire GMTX
        // after we release it via our own cond_wait.
        sa.ready = true;
        (rpt().cond_signal)(addr_of_mut!(sa.ready_cond));

        // Suspend until the scheduler picks us (releases GMTX atomically).
        let cond_ptr = addr_of_mut!(st().t(self_pt).suspend_cond);
        (rpt().cond_wait)(cond_ptr, addr_of_mut!(GMTX));
        gunlock();

        depth_exit();

        // Run the user's thread function with CALL_DEPTH back to 0 so that
        // user calls to pthread_* are correctly intercepted by the preload.
        let retval = routine(arg);
        // Scheduler cleanup then return naturally from the thread routine.
        // We intentionally do NOT call libc::pthread_exit here: doing so from
        // Rust conflicts with glibc's _Unwind_ForcedUnwind mechanism.
        do_thread_exit(self_pt);
        retval
    }
}

// ── rsched_pthread_create ─────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_create(
    thread:        *mut PthreadT,
    attr:          *const AttrT,
    start_routine: unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void,
    arg:           *mut libc::c_void,
) -> libc::c_int {
    ensure_init();

    let sa = Box::into_raw(Box::new(StartArg {
        routine: start_routine, arg,
        ready_cond: libc::PTHREAD_COND_INITIALIZER,
        ready: false,
    }));

    glock();
    let caller = my_pt();

    // Create the OS thread immediately — no pre-creation context_switch.
    //
    // A pre-creation switch can hand execution to a thread whose sanitizer
    // thread-start handshake (ASAN's GetArgs ↔ PostCreate, or TSAN's equivalent)
    // has not completed yet: PostCreate is called by the *parent* only after our
    // pthread_create returns, but a pre-creation switch freezes the parent
    // mid-call.  The ThreadCreate scheduling point is preserved below, after the
    // new thread has registered, so the scheduler still sees the event.
    #[cfg(feature = "asan")]
    let r = libc::pthread_create(thread, attr, trampoline, sa as *mut libc::c_void);
    #[cfg(not(feature = "asan"))]
    let r = (rpt().create)(thread, attr, trampoline, sa as *mut libc::c_void);

    // If thread creation failed, clean up and return immediately.
    if r != 0 {
        drop(Box::from_raw(sa));
        gunlock();
        return r;
    }

    // Wait for the new thread to register itself.  is_blocking prevents the
    // scheduler from choosing the caller during this window.
    st().t(caller).is_blocking = true;
    while !(*sa).ready {
        (rpt().cond_wait)(addr_of_mut!((*sa).ready_cond), addr_of_mut!(GMTX));
    }
    st().t(caller).is_blocking = false;
    drop(Box::from_raw(sa));

    // ThreadCreate scheduling point: now that the new thread is registered,
    // yield so the scheduler can explore interleavings from this point forward.
    // The new thread is excluded from scheduling (startup_done=false) so the
    // scheduler can only pick already-running threads whose sanitizer handshake
    // (ASAN PostCreate / TSAN equivalent) is already complete.
    st().set_event(caller, Event { instr_addr: return_address(), kind: EventKind::ThreadCreate });
    st().context_switch(caller);
    st().clear_event(caller);

    // Mark the new thread schedulable.  PostCreate will be called by our caller
    // (ASAN's __interceptor_pthread_create) after we return.  Between here and
    // that PostCreate, no glock is held so no context_switch can fire and wake
    // the new thread before the semaphore is posted.
    st().t(*thread).startup_done = true;

    gunlock();
    r
}

// ── rsched_pthread_join ───────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_join(
    thread: PthreadT,
    retval: *mut *mut libc::c_void,
) -> libc::c_int {
    ensure_init();

    glock();
    let caller = my_pt();
    st().context_switch(caller);

    if st().info.contains_key(&thread) {
        st().t(thread).joiner = Some(caller);
        st().t(caller).is_blocking = true;
        st().context_switch(caller);
    }

    gunlock();
    (rpt().join)(thread, retval)
}

// ── rsched_pthread_exit ───────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_exit(retval: *mut libc::c_void) -> ! {
    let _ = retval;
    ensure_init();
    do_thread_exit(my_pt());
    // Use the raw exit syscall to terminate this thread without going through
    // glibc's pthread_exit, which uses _Unwind_ForcedUnwind and conflicts with
    // Rust's own unwinding infrastructure.
    libc::syscall(libc::SYS_exit, 0i64);
    std::hint::unreachable_unchecked()
}

// ── rsched_pthread_mutex_lock ─────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_lock(lock: *mut MutexT) -> libc::c_int {
    ensure_init();
    let key = lock as usize;
    glock();
    let caller = my_pt();
    st().set_event(caller, Event { instr_addr: return_address(), kind: EventKind::LockAcq { lock: lock as *const _ } });
    let tid = st().info[&caller].tid;
    if !st().will_block(key, tid) { st().context_switch(caller); }
    st().mutex_lock(key, caller);
    // Inform TSAN that this thread has logically acquired the user mutex.
    // Called while GMTX is still held so the annotation is ordered relative
    // to the paired tsan_release in rsched_pthread_mutex_unlock.
    tsan_acquire(lock as *mut libc::c_void);
    st().clear_event(caller);
    gunlock();
    0
}

// ── rsched_pthread_mutex_trylock ──────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_trylock(lock: *mut MutexT) -> libc::c_int {
    ensure_init();
    let key = lock as usize;
    glock();
    let caller = my_pt();
    st().set_event(caller, Event { instr_addr: return_address(), kind: EventKind::LockAcq { lock: lock as *const _ } });
    st().context_switch(caller);
    let tid = st().info[&caller].tid;
    // Recursive trylock: same owner → EBUSY (zigsched line 1288).
    if let Some(m) = st().mutexes.get(&key) {
        if m.owner_tid == tid {
            st().clear_event(caller);
            gunlock();
            return libc::EBUSY;
        }
    }
    if st().will_block(key, tid) {
        st().clear_event(caller);
        gunlock();
        return libc::EBUSY;
    }
    st().mutex_lock(key, caller);
    st().clear_event(caller);
    gunlock();
    0
}

// ── rsched_pthread_mutex_unlock ───────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_unlock(lock: *mut MutexT) -> libc::c_int {
    ensure_init();
    let key = lock as usize;
    glock();
    let caller = my_pt();
    st().set_event(caller, Event { instr_addr: return_address(), kind: EventKind::LockRel { lock: lock as *const _ } });
    st().context_switch(caller);
    // Inform TSAN that this thread is releasing the user mutex before we
    // transfer virtual ownership to a waiter.
    tsan_release(lock as *mut libc::c_void);
    let r = st().mutex_unlock(key, caller);
    st().clear_event(caller);
    gunlock();
    r
}

// ── rsched_pthread_cond_wait ──────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_wait(
    cond: *mut CondT,
    lock: *mut MutexT,
) -> libc::c_int {
    ensure_init();
    let ckey = cond as usize;
    let lkey = lock as usize;
    glock();
    let caller = my_pt();
    let tid = st().info[&caller].tid;
    if !st().will_block(lkey, tid) { st().context_switch(caller); }
    let r = st().mutex_unlock(lkey, caller);
    if r != 0 { gunlock(); return r; }

    st().conds.entry(ckey)
        .or_insert(SCond { waiters: Vec::new() })
        .waiters.push(caller);
    st().t(caller).is_blocking = true;
    st().context_switch(caller);

    st().mutex_lock(lkey, caller);
    gunlock();
    0
}

// ── rsched_pthread_cond_signal ────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_signal(cond: *mut CondT) -> libc::c_int {
    ensure_init();
    let key = cond as usize;
    glock();
    let caller = my_pt();
    st().context_switch(caller);
    // Use an immutable borrow scoped to a block to get the waiter count,
    // then release it before calling the scheduler.
    let n = { st().conds.get(&key).map_or(0, |c| c.waiters.len()) };
    if n > 0 {
        let blocking = vec![false; n];
        let idx = st().scheduler.choose(&blocking).expect("rsched: non-empty cond waiter list");
        let w = st().conds.get_mut(&key).unwrap().waiters.remove(idx);
        st().t(w).is_blocking = false;
    }
    gunlock();
    0
}

// ── rsched_pthread_cond_broadcast ─────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_broadcast(cond: *mut CondT) -> libc::c_int {
    ensure_init();
    let key = cond as usize;
    glock();
    let caller = my_pt();
    st().context_switch(caller);
    if let Some(c) = st().conds.remove(&key) {
        for w in c.waiters { st().t(w).is_blocking = false; }
    }
    gunlock();
    0
}

// ── rsched_pthread_barrier_init ───────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_barrier_init(
    barrier: *mut BarrierT,
    attr:    *const libc::pthread_barrierattr_t,
    count:   libc::c_uint,
) -> libc::c_int {
    ensure_init();
    let key = barrier as usize;
    glock();
    let caller = my_pt();
    st().context_switch(caller);
    let r = (rpt().barrier_init)(barrier, attr, count);
    if r == 0 {
        st().barriers.insert(key, SBarrier { count, waiters: Vec::new() });
    }
    gunlock();
    r
}

// ── rsched_pthread_barrier_wait ───────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_barrier_wait(barrier: *mut BarrierT) -> libc::c_int {
    ensure_init();
    let key = barrier as usize;
    glock();
    let caller = my_pt();
    st().context_switch(caller);

    // Register as a waiter.
    {
        let b = match st().barriers.get_mut(&key) {
            Some(b) => b,
            None => { gunlock(); return libc::EINVAL; }
        };
        b.waiters.push(caller);
        st().t(caller).is_blocking = true;
    }

    // If we are the Nth thread, release all waiters.
    {
        let b = st().barriers.get_mut(&key).unwrap();
        if b.waiters.len() >= b.count as usize {
            let ws: Vec<PthreadT> = b.waiters.drain(..).collect();
            for w in ws { st().t(w).is_blocking = false; }
        }
    }

    // Yield to let a released waiter (or any runnable thread) run.
    // This also suspends us if we just released the barrier, giving others
    // a chance to exit their first context_switch inside barrier_wait.
    st().context_switch(caller);
    gunlock();
    0
}

// ── rsched_sched_yield ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sched_yield() -> libc::c_int {
    ensure_init();
    glock();
    let caller = my_pt();
    st().set_event(caller, Event { instr_addr: return_address(), kind: EventKind::SchedYield });
    st().context_switch(caller);
    st().clear_event(caller);
    gunlock();
    0
}

// ── rsched atomic operations ──────────────────────────────────────────────
//
// Each function is a cooperative scheduling point followed by the atomic
// operation itself.  The scheduling point fires *before* the memory access,
// so the scheduler may hand control to another thread between the yield and
// the actual load/store — exactly the interleaving window we want to explore.
//
// These are called by the _Generic dispatch in rsched_atomic.h, which
// overrides atomic_load_explicit / atomic_store_explicit from <stdatomic.h>.
//
// Layout compatibility:
//   *mut AtomicI32  ↔  _Atomic int *            (both 4 bytes, align 4)
//   *mut AtomicU32  ↔  _Atomic unsigned int *    (both 4 bytes, align 4)
// The ptr variants accept void * so _Generic's default: arm can pass any
// pointer-to-atomic-pointer without requiring a cast in the macro.

use std::sync::atomic::{AtomicI32, AtomicUsize};

/// Internal scheduling point for atomic memory operations.
/// `instr_addr` must be obtained via `return_address()` at the call site of the
/// exported atomic function so that it points into user code, not into rsched.
unsafe fn schedule_memop(instr_addr: u64, mem_addr: *const libc::c_void, size: usize, access: AccessKind) {
    ensure_init();
    glock();
    let caller = my_pt();
    st().set_event(caller, Event { instr_addr, kind: EventKind::MemOp { mem_addr, size, access } });
    st().context_switch(caller);
    st().clear_event(caller);
    gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_load_i32(ptr: *const AtomicI32) -> libc::c_int {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::Read);
    (*ptr).load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_store_i32(ptr: *mut AtomicI32, val: libc::c_int) {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::Write);
    (*ptr).store(val, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_load_u32(ptr: *const AtomicU32) -> libc::c_uint {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::Read);
    (*ptr).load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_store_u32(ptr: *mut AtomicU32, val: libc::c_uint) {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::Write);
    (*ptr).store(val, Ordering::SeqCst);
}

/// Load from an atomic pointer variable.
/// `ptr` is a type-erased pointer to any `T * _Atomic` variable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_load_ptr(ptr: *const libc::c_void) -> *mut libc::c_void {
    schedule_memop(return_address(), ptr, std::mem::size_of::<usize>(), AccessKind::Read);
    let atomic = &*(ptr as *const AtomicUsize);
    atomic.load(Ordering::SeqCst) as *mut libc::c_void
}

/// Store to an atomic pointer variable.
/// `ptr` is a type-erased pointer to any `T * _Atomic` variable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_store_ptr(
    ptr: *mut libc::c_void,
    val: *mut libc::c_void,
) {
    schedule_memop(return_address(), ptr, std::mem::size_of::<usize>(), AccessKind::Write);
    let atomic = &mut *(ptr as *mut AtomicUsize);
    atomic.store(val as usize, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_compare_exchange_i32(
    ptr: *mut AtomicI32,
    expected: *mut libc::c_int,
    desired: libc::c_int,
) -> bool {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::ReadWrite);
    match (*ptr).compare_exchange(*expected, desired, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => true,
        Err(current) => {
            *expected = current;
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_compare_exchange_u32(
    ptr: *mut AtomicU32,
    expected: *mut libc::c_uint,
    desired: libc::c_uint,
) -> bool {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::ReadWrite);
    match (*ptr).compare_exchange(*expected, desired, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => true,
        Err(current) => {
            *expected = current;
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_fetch_add_i32(
    ptr: *mut AtomicI32,
    val: libc::c_int,
) -> libc::c_int {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::ReadWrite);
    (*ptr).fetch_add(val, Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_fetch_add_u32(
    ptr: *mut AtomicU32,
    val: libc::c_uint,
) -> libc::c_uint {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::ReadWrite);
    (*ptr).fetch_add(val, Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_fetch_xor_i32(
    ptr: *mut AtomicI32,
    val: libc::c_int,
) -> libc::c_int {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::ReadWrite);
    (*ptr).fetch_xor(val, Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_fetch_xor_u32(
    ptr: *mut AtomicU32,
    val: libc::c_uint,
) -> libc::c_uint {
    schedule_memop(return_address(), ptr as *const _, 4, AccessKind::ReadWrite);
    (*ptr).fetch_xor(val, Ordering::SeqCst)
}
