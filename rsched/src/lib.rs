//! rsched – Random-walk cooperative thread scheduler, C-compatible library.
//!
//! Exports `rsched_pthread_*` / `rsched_sched_yield` symbols.
//! A companion C header (`include/rsched.h`) maps those over the standard
//! pthread names so that C programs can link against this library without
//! any LD_PRELOAD trickery.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(internal_features)]
#![allow(clippy::missing_safety_doc)]
#![feature(linkage)]
#![feature(link_llvm_intrinsics)]

use std::cell::RefCell;
use std::collections::{BTreeMap as HashMap, BTreeSet as HashSet};
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

mod scheduler;
use scheduler::{Scheduler, SchedulerImpl};

mod event;
pub use event::{AccessKind, Event, EventKind};

mod fuzzing;
mod seccomp;
mod task_provider;
mod tsan;
use task_provider::{
    ParkingHandle, ProcessTaskProvider, SwitchMode, SwitchResult, TaskChoice, TaskHandle,
    deactivate_current_domain_tasks, default_task_provider, exit_current_domain,
};

// ── Type aliases ──────────────────────────────────────────────────────────

type PthreadT = libc::pthread_t;
type MutexT = libc::pthread_mutex_t;
type CondT = libc::pthread_cond_t;
type BarrierT = libc::pthread_barrier_t;
type AttrT = libc::pthread_attr_t;
type MutexAttrT = libc::pthread_mutexattr_t;
type StartRoutine = unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void;

// ── Thread descriptor ─────────────────────────────────────────────────────

struct Thread {
    task: Option<TaskHandle>,
    tid: libc::pid_t,
    pthread: PthreadT,
    /// Condition the scheduler uses to suspend/resume this thread.
    /// All suspensions wait on this cond with GMTX held.
    suspend_cond: CondT,
    /// Thread waiting on this one via pthread_join.
    joiner: Option<PthreadT>,
    /// The user start routine has returned.  Keep the record around until
    /// reinit because thread/runtime teardown can still touch pthread state.
    is_exited: bool,
    /// What this thread is about to do at the next scheduling point.
    /// Set before each context switch, cleared after the operation completes.
    pub next_event: Option<Event>,
}

impl Thread {
    fn new(pt: PthreadT) -> Self {
        Self::new_with_tid(pt, unsafe {
            libc::syscall(libc::SYS_gettid) as libc::pid_t
        })
    }

    fn new_with_tid(pt: PthreadT, tid: libc::pid_t) -> Self {
        Thread {
            task: None,
            tid,
            pthread: pt,
            suspend_cond: libc::PTHREAD_COND_INITIALIZER,
            joiner: None,
            is_exited: false,
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
    scheduler: SchedulerImpl,
    task_provider: ProcessTaskProvider,
    threads: Vec<PthreadT>,
    info: HashMap<PthreadT, Box<Thread>>,
    mutexes: HashMap<usize, SMutex>,
    recursive_mutexes: HashSet<usize>,
    recursive_mutex_attrs: HashSet<usize>,
    conds: HashMap<usize, SCond>,
    barriers: HashMap<usize, SBarrier>,
    /// A child thread created but not yet processed at a scheduling point.
    /// rsched_pthread_create stores the (StartArg*, child pthread_t) here and
    /// returns immediately so sanitiser post-create hooks can run.  The next
    /// context_switch call drains the slot: it waits for the child's ready_cond
    /// signal (if not already fired), frees the StartArg, and marks the child's
    /// startup_done = true.
    pending_child: Option<(*mut StartArg, PthreadT)>,
}

impl State {
    fn new(seed: u64) -> Self {
        let scheduler = SchedulerImpl::new(seed);
        let task_provider = default_task_provider();
        State {
            scheduler,
            task_provider,
            threads: Vec::new(),
            info: HashMap::new(),
            mutexes: HashMap::new(),
            recursive_mutexes: HashSet::new(),
            recursive_mutex_attrs: HashSet::new(),
            conds: HashMap::new(),
            barriers: HashMap::new(),
            pending_child: None,
        }
    }

    unsafe fn add(&mut self, mut t: Thread) {
        let pt = t.pthread;
        t.task = Some(self.task_provider.register_task(pt));
        self.threads.push(pt);
        self.info.insert(pt, Box::new(t));
    }

    fn remove(&mut self, pt: PthreadT) {
        self.threads.retain(|&x| x != pt);
        if let Some(t) = self.info.get_mut(&pt) {
            t.is_exited = true;
            unsafe {
                t.task
                    .expect("rsched: registered thread has no task handle")
                    .deactivate();
            }
        }
    }

    fn t(&mut self, pt: PthreadT) -> &mut Thread {
        self.info
            .get_mut(&pt)
            .expect("rsched: unknown thread")
            .as_mut()
    }

    fn task(&self, pt: PthreadT) -> TaskHandle {
        self.info
            .get(&pt)
            .and_then(|t| t.task)
            .expect("rsched: unknown task")
    }

    fn choose_index(&mut self, blocking: &[bool]) -> Option<usize> {
        self.scheduler.choose(blocking)
    }

    /// Complete a deferred child startup handshake.
    ///
    /// `rsched_pthread_create` cannot always wait for the child immediately
    /// because sanitizer runtimes have post-create handshakes to complete.
    /// Before the next scheduling decision, or before storing another pending
    /// child, we wait until the child has registered and then mark it runnable.
    unsafe fn drain_pending_child(&mut self) {
        if let Some((sa, child_pt)) = self.pending_child.take() {
            while !(*sa).ready {
                self.task_provider
                    .park(ParkingHandle::new(addr_of_mut!((*sa).ready_cond)));
            }
            drop(Box::from_raw(sa));
            if self.info.contains_key(&child_pt) {
                self.task(child_pt).set_startup_done(true);
            }
        }
    }

    /// Signal `next`'s suspend_cond and, if `suspend_caller` and next≠caller,
    /// suspend `caller` by waiting on its own cond (releases GMTX atomically).
    /// Must be called with GMTX held.
    unsafe fn wake_local(&mut self, next: PthreadT, suspend_caller: bool, caller: PthreadT) {
        if next != caller {
            let cond_ptr = addr_of_mut!(self.info.get_mut(&next).unwrap().suspend_cond);
            self.task_provider.wake(ParkingHandle::new(cond_ptr));
            if suspend_caller {
                let my_cond = addr_of_mut!(self.info.get_mut(&caller).unwrap().suspend_cond);
                self.task_provider.park(ParkingHandle::new(my_cond));
            }
        }
    }

    unsafe fn mark_waiting(&mut self, pt: PthreadT, is_waiting: bool) {
        if self.info.contains_key(&pt) {
            self.task(pt).set_waiting(is_waiting);
        }
    }

    unsafe fn park_if_blocked(&mut self, caller: PthreadT) {
        if self.task(caller).is_blocking() {
            self.task(caller).set_waiting(true);
            let cond = addr_of_mut!(self.t(caller).suspend_cond);
            self.task_provider.park(ParkingHandle::new(cond));
            self.task(caller).set_waiting(false);
        }
    }

    /// Pick a random non-blocking thread and hand control to it, suspending caller.
    unsafe fn context_switch(&mut self, caller: PthreadT) {
        // Drain any pending child registration.  The cond_wait releases GMTX
        // while waiting, so the child's trampoline can acquire it to register.
        self.drain_pending_child();

        if !self.threads.contains(&caller) {
            return;
        }

        if !self.task(caller).startup_done() {
            self.task(caller).set_startup_done(true);
        }

        let event = self.info.get(&caller).and_then(|t| t.next_event);
        self.scheduler.on_event(event.as_ref());
        if event.is_some() {
            self.clear_event(caller);
        }

        let avoid_self = self.scheduler.avoid_self_on_stutter()
            && (event.is_none()
                || matches!(
                    event,
                    Some(Event {
                        kind: EventKind::SchedYield,
                        ..
                    })
                ));

        if let Some(choice) = self.choose_task(caller, avoid_self) {
            self.mark_waiting(caller, true);
            if let SwitchResult::WakeLocal {
                pthread,
                suspend_caller,
            } = self
                .task_provider
                .switch_task(choice, caller, SwitchMode::SuspendCurrent)
            {
                self.wake_local(pthread, suspend_caller, caller);
            }
            self.mark_waiting(caller, false);
        }
    }

    unsafe fn choose_task(&mut self, caller: PthreadT, avoid_self: bool) -> Option<TaskChoice> {
        if self.info.contains_key(&caller) {
            self.task(caller).set_waiting(true);
        }
        let scheduler = &mut self.scheduler;
        let mut choose_index = |n: usize| {
            let blocking = vec![false; n];
            scheduler.choose(&blocking)
        };
        if let Some(choice) =
            self.task_provider
                .choose_domain_task(caller, avoid_self, &mut choose_index)
        {
            return Some(choice);
        }
        if self.info.contains_key(&caller) {
            self.task(caller).set_waiting(false);
        }

        let current = self.task_provider.current_domain();
        {
            let mut tasks = Vec::new();
            for pt in self.threads.iter().copied() {
                let t = self.info[&pt].as_ref();
                let task = t
                    .task
                    .expect("rsched: registered thread has no task handle");
                if !task.is_blocking() && task.startup_done() && (pt == caller || task.is_waiting())
                {
                    tasks.push(TaskChoice {
                        creation_idx: Some(task.creation_idx()),
                        domain: current,
                        pthread: pt,
                    });
                }
            }
            if avoid_self && tasks.len() > 1 {
                tasks.retain(|task| task.pthread != caller);
            }
            let blocking = vec![false; tasks.len()];
            self.choose_index(&blocking).map(|idx| tasks[idx])
        }
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

    fn is_recursive_mutex(&self, key: usize) -> bool {
        self.recursive_mutexes.contains(&key)
    }

    unsafe fn mutex_lock(&mut self, key: usize, caller: PthreadT) {
        let tid = self.info[&caller].tid;
        loop {
            let e = self.mutexes.entry(key).or_insert(SMutex {
                owner_tid: tid,
                recursive_count: 0,
                waiters: Vec::new(),
            });
            if e.owner_tid == tid {
                e.recursive_count += 1;
                return;
            }
            if !e.waiters.contains(&caller) {
                e.waiters.push(caller);
            }
            self.task(caller).set_blocking(true);
            self.context_switch(caller);
            // context_switch may have returned immediately (None path) without
            // suspending us, e.g. when the mutex owner is running but not in
            // rsched's cond_wait (common in TSAN mode where threads run freely).
            // In that case we must release GMTX by doing a real cond_wait so
            // the owner can acquire GMTX to call mutex_unlock.
            // mutex_unlock will signal our suspend_cond after transferring
            // ownership, so this wait is always eventually resolved.
            self.park_if_blocked(caller);
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
            if m.owner_tid != tid {
                return libc::EPERM;
            }
            m.recursive_count -= 1;
            if m.recursive_count > 0 {
                return 0;
            }
            m.waiters.len()
        }; // borrow of self.mutexes released here
        if waiter_count == 0 {
            self.mutexes.remove(&key);
            return 0;
        }
        // Phase 2: pick a random waiter (needs self.scheduler, no mutexes borrow active).
        let blocking = vec![false; waiter_count];
        let idx = self
            .choose_index(&blocking)
            .expect("rsched: non-empty waiter list");
        // Phase 3: transfer ownership to chosen waiter.
        let waiter = self.mutexes.get_mut(&key).unwrap().waiters.remove(idx);
        let waiter_tid = self.info[&waiter].tid;
        self.mutexes.get_mut(&key).unwrap().owner_tid = waiter_tid;
        self.task(waiter).set_blocking(false);
        0
    }
}

// ── pthread mutex metadata ─────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_note_pthread_mutexattr_init(attr: *mut MutexAttrT) {
    ensure_init();
    rsched_glock();
    st().recursive_mutex_attrs.remove(&(attr as usize));
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_note_pthread_mutexattr_settype(
    attr: *mut MutexAttrT,
    kind: libc::c_int,
) {
    ensure_init();
    rsched_glock();
    let key = attr as usize;
    if kind == libc::PTHREAD_MUTEX_RECURSIVE {
        st().recursive_mutex_attrs.insert(key);
    } else {
        st().recursive_mutex_attrs.remove(&key);
    }
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_note_pthread_mutexattr_destroy(attr: *mut MutexAttrT) {
    ensure_init();
    rsched_glock();
    st().recursive_mutex_attrs.remove(&(attr as usize));
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_note_pthread_mutex_init(
    lock: *mut MutexT,
    attr: *const MutexAttrT,
) {
    ensure_init();
    rsched_glock();
    let key = lock as usize;
    if !attr.is_null() && st().recursive_mutex_attrs.contains(&(attr as usize)) {
        st().recursive_mutexes.insert(key);
    } else {
        st().recursive_mutexes.remove(&key);
    }
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_note_pthread_mutex_destroy(lock: *mut MutexT) {
    ensure_init();
    rsched_glock();
    let key = lock as usize;
    st().recursive_mutexes.remove(&key);
    st().mutexes.remove(&key);
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutexattr_init(attr: *mut MutexAttrT) -> libc::c_int {
    let r = with_internal_depth(|| libc::pthread_mutexattr_init(attr));
    if r == 0 {
        rsched_note_pthread_mutexattr_init(attr);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutexattr_settype(
    attr: *mut MutexAttrT,
    kind: libc::c_int,
) -> libc::c_int {
    let r = with_internal_depth(|| libc::pthread_mutexattr_settype(attr, kind));
    if r == 0 {
        rsched_note_pthread_mutexattr_settype(attr, kind);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutexattr_destroy(attr: *mut MutexAttrT) -> libc::c_int {
    let r = with_internal_depth(|| libc::pthread_mutexattr_destroy(attr));
    if r == 0 {
        rsched_note_pthread_mutexattr_destroy(attr);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_init(
    lock: *mut MutexT,
    attr: *const MutexAttrT,
) -> libc::c_int {
    let r = with_internal_depth(|| libc::pthread_mutex_init(lock, attr));
    if r == 0 {
        rsched_note_pthread_mutex_init(lock, attr);
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_destroy(lock: *mut MutexT) -> libc::c_int {
    let r = with_internal_depth(|| libc::pthread_mutex_destroy(lock));
    if r == 0 {
        rsched_note_pthread_mutex_destroy(lock);
    }
    r
}

// ── Globals ───────────────────────────────────────────────────────────────

static mut GMTX: MutexT = libc::PTHREAD_MUTEX_INITIALIZER;
static mut STATE: Option<State> = None;
static INITED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static MY_PT: RefCell<PthreadT> = const { RefCell::new(unsafe { std::mem::zeroed() }) };
}

fn my_pt() -> PthreadT {
    #[cfg(feature = "instrumented-libc")]
    {
        unsafe { libc::pthread_self() }
    }
    #[cfg(not(feature = "instrumented-libc"))]
    {
        MY_PT.with(|c| *c.borrow())
    }
}

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
    mutex_lock: unsafe extern "C" fn(*mut MutexT) -> libc::c_int,
    mutex_unlock: unsafe extern "C" fn(*mut MutexT) -> libc::c_int,
    cond_wait: unsafe extern "C" fn(*mut CondT, *mut MutexT) -> libc::c_int,
    cond_signal: unsafe extern "C" fn(*mut CondT) -> libc::c_int,
    create: unsafe extern "C" fn(
        *mut PthreadT,
        *const AttrT,
        unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void,
        *mut libc::c_void,
    ) -> libc::c_int,
    join: unsafe extern "C" fn(PthreadT, *mut *mut libc::c_void) -> libc::c_int,
}

static REAL_PT: std::sync::OnceLock<RealPt> = std::sync::OnceLock::new();

#[cfg(feature = "instrumented-libc")]
unsafe extern "C" {
    #[link_name = "__rsched_real_pthread_mutex_lock"]
    fn instrumented_libc_mutex_lock(mutex: *mut MutexT) -> libc::c_int;
    #[link_name = "__rsched_real_pthread_mutex_unlock"]
    fn instrumented_libc_mutex_unlock(mutex: *mut MutexT) -> libc::c_int;
    #[link_name = "__rsched_real_pthread_cond_wait"]
    fn instrumented_libc_cond_wait(cond: *mut CondT, mutex: *mut MutexT) -> libc::c_int;
    #[link_name = "__rsched_real_pthread_cond_signal"]
    fn instrumented_libc_cond_signal(cond: *mut CondT) -> libc::c_int;
    #[link_name = "__rsched_real_pthread_create"]
    fn instrumented_libc_create(
        thread: *mut PthreadT,
        attr: *const AttrT,
        start: unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void,
        arg: *mut libc::c_void,
    ) -> libc::c_int;
    #[link_name = "__rsched_real_pthread_join"]
    fn instrumented_libc_join(thread: PthreadT, retval: *mut *mut libc::c_void) -> libc::c_int;
}

unsafe fn rpt() -> &'static RealPt {
    REAL_PT.get_or_init(|| {
        #[cfg(feature = "instrumented-libc")]
        {
            RealPt {
                mutex_lock: instrumented_libc_mutex_lock,
                mutex_unlock: instrumented_libc_mutex_unlock,
                cond_wait: instrumented_libc_cond_wait,
                cond_signal: instrumented_libc_cond_signal,
                create: instrumented_libc_create,
                join: instrumented_libc_join,
            }
        }

        #[cfg(not(feature = "instrumented-libc"))]
        {
            // Try the traditional stub first; on glibc >= 2.34 the symbols live in
            // libc.so.6 and libpthread.so.0 is a forwarding stub that still responds
            // to dlsym correctly.  RTLD_NOLOAD avoids loading anything new.
            let mut lib = libc::dlopen(
                c"libpthread.so.0".as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_NOLOAD,
            );
            if lib.is_null() {
                lib = libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD);
            }
            assert!(
                !lib.is_null(),
                "rsched: cannot resolve libpthread/libc via dlopen"
            );

            // Helper: look up one symbol and transmute to the target function-pointer
            // type.  Function pointers and data pointers share the same width on every
            // platform rsched targets, so the transmute is sound.
            unsafe fn sym<T: Copy>(lib: *mut libc::c_void, name: &[u8]) -> T {
                let p = libc::dlsym(lib, name.as_ptr() as *const _);
                assert!(!p.is_null(), "rsched: dlsym returned null");
                std::mem::transmute_copy::<*mut libc::c_void, T>(&p)
            }

            RealPt {
                mutex_lock: sym(lib, b"pthread_mutex_lock\0"),
                mutex_unlock: sym(lib, b"pthread_mutex_unlock\0"),
                cond_wait: sym(lib, b"pthread_cond_wait\0"),
                cond_signal: sym(lib, b"pthread_cond_signal\0"),
                create: sym(lib, b"pthread_create\0"),
                join: sym(lib, b"pthread_join\0"),
            }
        }
    })
}

pub(crate) unsafe fn thread_glock() {
    with_internal_depth(|| (rpt().mutex_lock)(addr_of_mut!(GMTX)));
}
pub(crate) unsafe fn thread_gunlock() {
    with_internal_depth(|| (rpt().mutex_unlock)(addr_of_mut!(GMTX)));
}

unsafe fn rsched_glock() {
    st().task_provider.global_lock();
    tsan::sync_acquire();
}

unsafe fn rsched_gunlock() {
    tsan::sync_release();
    st().task_provider.global_unlock();
}

pub(crate) unsafe fn thread_cond_wait(cond: *mut CondT) {
    tsan::sync_release();
    with_internal_depth(|| (rpt().cond_wait)(cond, addr_of_mut!(GMTX)));
    tsan::sync_acquire();
}

#[cfg(not(feature = "instrumented-libc"))]
extern "C" fn process_atexit() {
    unsafe {
        rsched_process_exit();
    }
}

pub(crate) unsafe fn reset_local_state_after_fork() {
    STATE = None;
    INITED.store(false, Ordering::SeqCst);
    rsched_init();
}

#[cfg(feature = "tsan")]
pub unsafe fn rsched_is_tsan_thread_start(start: StartRoutine, arg: *mut libc::c_void) -> bool {
    tsan::is_thread_start(start, arg)
}

#[cfg(feature = "tsan")]
pub unsafe fn rsched_is_tsan_background_start(start: StartRoutine, arg: *mut libc::c_void) -> bool {
    tsan::is_background_start(start, arg)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_before_fork() {
    ensure_init();
    st().task_provider.before_fork();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_after_fork_parent(child: libc::pid_t) {
    if st().task_provider.after_fork_parent(child) {
        rsched_glock();
        let caller = my_pt();
        st().context_switch(caller);
        rsched_gunlock();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_after_fork_child() {
    st().task_provider.after_fork_child();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_process_exit() {
    if !INITED.load(Ordering::Acquire) {
        exit_current_domain(None);
        return;
    }

    rsched_glock();
    let next = st().choose_task(0 as PthreadT, false);
    st().task_provider.process_exit(next);
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fork() -> libc::pid_t {
    ensure_init();
    st().task_provider.fork()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_syscall(
    number: libc::c_long,
    a0: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
) -> libc::c_long {
    if number == libc::SYS_clone {
        ensure_init();
        return st().task_provider.clone_process(a0, a1, a2, a3, a4).into();
    }

    libc::syscall(number, a0, a1, a2, a3, a4, a5)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_raw_syscall(
    number: libc::c_long,
    a0: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
) -> libc::c_long {
    let result = seccomp::raw_syscall6(number, a0, a1, a2, a3, a4, a5);
    if (-4095..0).contains(&result) {
        *libc::__errno_location() = -result as libc::c_int;
        -1
    } else {
        result
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_execv(
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
) -> libc::c_int {
    ensure_init();
    st().task_provider.execv(path, argv)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_execve(
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
) -> libc::c_int {
    ensure_init();
    st().task_provider.execve(path, argv, envp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_waitpid(
    pid: libc::pid_t,
    status: *mut libc::c_int,
    options: libc::c_int,
) -> libc::pid_t {
    ensure_init();
    if options & libc::WNOHANG != 0 {
        return with_internal_depth(|| libc::waitpid(pid, status, options));
    }

    loop {
        let r = with_internal_depth(|| libc::waitpid(pid, status, options | libc::WNOHANG));
        if r != 0 {
            return r;
        }
        rsched_sched_yield();
    }
}

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

#[cfg(feature = "instrumented-libc")]
static INSTRUMENTED_LIBC_READY: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "instrumented-libc")]
static INSTRUMENTED_DEPTH_TIDS: [AtomicI32; 128] = [const { AtomicI32::new(0) }; 128];
#[cfg(feature = "instrumented-libc")]
static INSTRUMENTED_DEPTHS: [AtomicU32; 128] = [const { AtomicU32::new(0) }; 128];

#[cfg(feature = "instrumented-libc")]
#[unsafe(no_mangle)]
pub extern "C" fn rsched_activate_instrumented_libc() {
    INSTRUMENTED_LIBC_READY.store(true, Ordering::Release);
}

#[cfg(feature = "instrumented-libc")]
fn instrumented_depth_slot() -> usize {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    for (idx, seen) in INSTRUMENTED_DEPTH_TIDS.iter().enumerate() {
        let value = seen.load(Ordering::Acquire);
        if value == tid {
            return idx;
        }
        if value == 0
            && seen
                .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return idx;
        }
    }
    0
}

#[cfg(feature = "instrumented-libc")]
fn instrumented_depth_fetch_add(delta: u32) -> u32 {
    INSTRUMENTED_DEPTHS[instrumented_depth_slot()].fetch_add(delta, Ordering::Acquire)
}

#[cfg(feature = "instrumented-libc")]
fn instrumented_depth_fetch_sub(delta: u32) -> u32 {
    INSTRUMENTED_DEPTHS[instrumented_depth_slot()].fetch_sub(delta, Ordering::Release)
}

#[cfg(feature = "instrumented-libc")]
fn instrumented_depth_load() -> u32 {
    INSTRUMENTED_DEPTHS[instrumented_depth_slot()].load(Ordering::Relaxed)
}

#[inline]
fn instrumented_libc_ready() -> bool {
    #[cfg(feature = "instrumented-libc")]
    {
        INSTRUMENTED_LIBC_READY.load(Ordering::Acquire)
    }
    #[cfg(not(feature = "instrumented-libc"))]
    {
        true
    }
}

/// Increment depth; return true iff this is the outermost (non-reentrant) call.
/// Called by the preload interceptors on every entry.
#[unsafe(no_mangle)]
pub fn rsched_try_enter() -> bool {
    if !instrumented_libc_ready() {
        return false;
    }
    #[cfg(feature = "instrumented-libc")]
    {
        instrumented_depth_fetch_add(1) == 0
    }
    #[cfg(not(feature = "instrumented-libc"))]
    {
        CALL_DEPTH.with(|d| d.fetch_add(1, Ordering::Acquire) == 0)
    }
}

/// Decrement depth. Called by the preload interceptors on every exit.
#[unsafe(no_mangle)]
pub fn rsched_exit() {
    if !instrumented_libc_ready() {
        return;
    }
    #[cfg(feature = "instrumented-libc")]
    instrumented_depth_fetch_sub(1);
    #[cfg(not(feature = "instrumented-libc"))]
    CALL_DEPTH.with(|d| {
        d.fetch_sub(1, Ordering::Release);
    });
}

// Ubuntu's static libgcc_eh references this glibc loader helper. Instrumented
// musl builds use panic=abort, so stack unwinding never reaches this fallback.
#[cfg(feature = "instrumented-libc")]
#[unsafe(no_mangle)]
pub extern "C" fn _dl_find_object(
    _address: *const libc::c_void,
    _result: *mut libc::c_void,
) -> libc::c_int {
    -1
}

/// Increment depth without checking. Used internally by trampoline/do_thread_exit.
#[inline]
fn depth_enter() {
    #[cfg(feature = "instrumented-libc")]
    instrumented_depth_fetch_add(1);
    #[cfg(not(feature = "instrumented-libc"))]
    CALL_DEPTH.with(|d| {
        d.fetch_add(1, Ordering::Acquire);
    });
}

/// Decrement depth. Paired with depth_enter().
#[inline]
fn depth_exit() {
    #[cfg(feature = "instrumented-libc")]
    instrumented_depth_fetch_sub(1);
    #[cfg(not(feature = "instrumented-libc"))]
    CALL_DEPTH.with(|d| {
        d.fetch_sub(1, Ordering::Release);
    });
}

#[inline]
fn is_in_rsched() -> bool {
    #[cfg(feature = "instrumented-libc")]
    {
        instrumented_depth_load() > 0
    }
    #[cfg(not(feature = "instrumented-libc"))]
    {
        CALL_DEPTH.with(|d| d.load(Ordering::Relaxed) > 0)
    }
}

#[inline]
unsafe fn with_internal_depth<T>(f: impl FnOnce() -> T) -> T {
    depth_enter();
    let r = f();
    depth_exit();
    r
}

unsafe fn st() -> &'static mut State {
    (*addr_of_mut!(STATE))
        .as_mut()
        .expect("rsched not initialised")
}

#[cfg(feature = "instrumented-libc")]
unsafe fn seed_from_env() -> u64 {
    let ptr = libc::getenv(c"RANDOM_SEED".as_ptr());
    if ptr.is_null() {
        return 0x12345678abcdu64;
    }

    let mut seed = 0u64;
    let mut cursor = ptr.cast::<u8>();
    while cursor.read().is_ascii_digit() {
        seed = seed
            .saturating_mul(10)
            .saturating_add(u64::from(cursor.read() - b'0'));
        cursor = cursor.add(1);
    }
    seed
}

#[cfg(not(feature = "instrumented-libc"))]
fn seed_from_env() -> u64 {
    std::env::var("RANDOM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x12345678abcdu64)
}

// ── rsched_init ───────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_init() {
    if INITED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let seed = seed_from_env();
    STATE = Some(State::new(seed));
    let first_process = st().task_provider.current_domain() < 0;
    st().task_provider.register_current_domain(first_process);
    #[cfg(not(feature = "instrumented-libc"))]
    if first_process {
        with_internal_depth(|| libc::atexit(process_atexit));
    }

    let self_pt = libc::pthread_self();
    #[cfg(not(feature = "instrumented-libc"))]
    MY_PT.with(|c| *c.borrow_mut() = self_pt);
    rsched_glock();
    st().add(Thread::new(self_pt));
    st().task(self_pt).set_startup_done(true);
    rsched_gunlock();
    seccomp::install_tripwire_if_requested();
}

fn ensure_init() {
    if !INITED.load(Ordering::SeqCst) {
        unsafe {
            rsched_init();
        }
    }
}

// ── rsched_reinit ─────────────────────────────────────────────────────────

/// Reset the scheduler with a new seed.  Must be called from the main thread
/// only, after all previously-spawned threads have been joined.
/// Used by integration tests to run multiple scenarios in one process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_reinit(seed: u64) {
    if let Some(state) = (*addr_of_mut!(STATE)).as_mut() {
        state.scheduler.finish();
    }
    deactivate_current_domain_tasks();

    // Drop any existing state (previous test run's threads/mutexes/etc.).
    STATE = None;
    STATE = Some(State::new(seed));
    INITED.store(true, Ordering::SeqCst);
    if st().task_provider.current_domain() < 0 {
        st().task_provider.register_current_domain(true);
    }

    let self_pt = libc::pthread_self();
    #[cfg(not(feature = "instrumented-libc"))]
    MY_PT.with(|c| *c.borrow_mut() = self_pt);
    rsched_glock();
    st().add(Thread::new(self_pt));
    st().task(self_pt).set_startup_done(true);
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_dfs_reset() {
    scheduler::dfs_reset();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_dfs_has_next() -> bool {
    scheduler::dfs_has_next()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_dfs_finish_current() {
    if let Some(state) = (*addr_of_mut!(STATE)).as_mut() {
        state.scheduler.finish();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_dfs_completed_schedules() -> usize {
    scheduler::dfs_completed_schedules()
}

// ── Thread trampoline ─────────────────────────────────────────────────────

pub(crate) struct StartArg {
    pub(crate) routine: StartRoutine,
    pub(crate) arg: *mut libc::c_void,
    /// Signaled (with GMTX held) once the new thread has registered itself.
    pub(crate) ready_cond: CondT,
    pub(crate) ready: bool,
}
unsafe impl Send for StartArg {}

/// Shared cleanup logic for thread exit.  Must be called with GMTX *not* held.
pub(crate) unsafe fn do_thread_exit(caller: PthreadT) {
    // Raise CALL_DEPTH before acquiring GMTX so that any libc-internal PLT
    // re-entry through our preload interceptors is treated as non-outermost.
    depth_enter();
    rsched_glock();
    let joiner_opt = st().info.get(&caller).and_then(|t| t.joiner);
    if let Some(joiner) = joiner_opt {
        st().task(joiner).set_blocking(false);
    }
    st().remove(caller);
    let mut local_tasks = Vec::new();
    for pt in st().threads.iter().copied() {
        let task = st().task(pt);
        if !task.is_blocking() && task.startup_done() && task.is_waiting() {
            local_tasks.push(pt);
        }
    }
    if !local_tasks.is_empty() {
        let blocking = vec![false; local_tasks.len()];
        let idx = st()
            .choose_index(&blocking)
            .expect("rsched: non-empty local exit task list");
        let next = local_tasks[idx];
        let cond_ptr = addr_of_mut!(st().t(next).suspend_cond);
        st().task_provider.wake(ParkingHandle::new(cond_ptr));
    } else if let Some(choice) = st().choose_task(0 as PthreadT, false)
        && let SwitchResult::WakeLocal { pthread, .. } =
            st().task_provider
                .switch_task(choice, 0 as PthreadT, SwitchMode::ReleaseCurrent)
        && st().info.contains_key(&pthread)
    {
        let cond_ptr = addr_of_mut!(st().t(pthread).suspend_cond);
        st().task_provider.wake(ParkingHandle::new(cond_ptr));
    }
    rsched_gunlock();
    depth_exit();
}

// Safe fn required because libc::pthread_create takes a safe fn pointer.
#[allow(dead_code)]
pub(crate) extern "C" fn trampoline(raw: *mut libc::c_void) -> *mut libc::c_void {
    unsafe {
        let sa = &mut *(raw as *mut StartArg);
        let routine = sa.routine;
        let arg = sa.arg;

        let self_pt = libc::pthread_self();
        #[cfg(not(feature = "instrumented-libc"))]
        MY_PT.with(|c| *c.borrow_mut() = self_pt);

        // Raise CALL_DEPTH before calling any rpt() primitive so that any
        // libc-internal PLT re-entry (e.g. cond_wait releasing GMTX via
        // pthread_mutex_unlock through our preload's interceptor) is seen as
        // non-outermost and forwarded to RTLD_NEXT instead of routing back
        // into rsched (which would try to re-acquire GMTX → deadlock).
        depth_enter();

        // Acquire GMTX (creator released it via pthread_cond_wait below).
        st().task_provider.global_lock();
        st().t(self_pt).tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;

        // Signal parent that we are registered (parent may or may not be waiting).
        sa.ready = true;
        st().task_provider
            .wake(ParkingHandle::new(addr_of_mut!(sa.ready_cond)));

        // Under TSAN the parent returns from rsched_pthread_create immediately
        // and the sanitiser runs its handshake (p->sync.Wait / p->start.Post)
        // before the parent issues any further pthread call.  If we entered an
        // initial cond_wait here, the child would never call the sanitiser's
        // start function (which does p->sync.Post) → deadlock.  Instead we
        // release GMTX and let the child run the user function immediately; the
        // first user pthread call will enter rsched cooperative scheduling.
        //
        // Without TSAN we keep the initial cond_wait so cooperative scheduling
        // starts from the very beginning of the new thread's lifetime.
        #[cfg(not(feature = "tsan"))]
        {
            // Suspend until the scheduler picks us (releases GMTX atomically).
            st().task(self_pt).set_waiting(true);
            let cond_ptr = addr_of_mut!(st().t(self_pt).suspend_cond);
            st().task_provider.park(ParkingHandle::new(cond_ptr));
            st().task(self_pt).set_waiting(false);
        }

        st().task_provider.global_unlock();
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
    thread: *mut PthreadT,
    attr: *const AttrT,
    start_routine: StartRoutine,
    arg: *mut libc::c_void,
) -> libc::c_int {
    ensure_init();

    #[cfg(feature = "tsan")]
    let tsan_gate = if tsan::is_thread_start(start_routine, arg) {
        Some(tsan::prepare_start_gate(arg))
    } else {
        None
    };

    let sa = Box::into_raw(Box::new(StartArg {
        routine: start_routine,
        arg,
        ready_cond: libc::PTHREAD_COND_INITIALIZER,
        ready: false,
    }));

    rsched_glock();

    // `pending_child` is intentionally a single slot.  Consecutive
    // pthread_create calls can happen before any ordinary scheduling point, so
    // finish the previous child's registration before creating another OS
    // thread.  Doing this before the real create keeps registration order
    // deterministic instead of letting the old and new children race to GMTX.
    st().drain_pending_child();

    let r = st().task_provider.create(thread, attr, sa);

    if r != 0 {
        drop(Box::from_raw(sa));
        #[cfg(feature = "tsan")]
        if let Some(gate) = tsan_gate {
            tsan::restore_start_gate(arg, gate);
        }
        rsched_gunlock();
        return r;
    }

    let child_tid = st().task_provider.task_tid(*thread);
    st().add(Thread::new_with_tid(*thread, child_tid));
    if st().task_provider.starts_waiting() {
        st().task(*thread).set_waiting(true);
    }

    // Return immediately without waiting for the child to register.  Sanitiser
    // runtimes (TSAN, ASAN) run their own post-create hooks (e.g. TSAN's
    // p->sync.Wait / p->start.Post handshake) between REAL(pthread_create)
    // returning and any subsequent pthread call.  If we block here waiting for
    // the child's ready_cond the child cannot make progress because it is
    // waiting for the sanitiser to complete its handshake — deadlock.
    //
    // Instead we stash (sa, child_pthread_t) in State::pending_child.  The
    // *next* scheduling point (any context_switch call) will drain the slot:
    // it waits for the child's ready_cond, frees sa, and marks startup_done.
    st().set_event(
        my_pt(),
        Event {
            instr_addr: return_address(),
            kind: EventKind::ThreadCreate,
        },
    );
    st().pending_child = Some((sa, *thread));
    rsched_gunlock();
    r
}

// ── rsched_pthread_join ───────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_join(
    thread: PthreadT,
    retval: *mut *mut libc::c_void,
) -> libc::c_int {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    st().context_switch(caller);

    if st().threads.contains(&thread) {
        st().t(thread).joiner = Some(caller);
        st().task(caller).set_blocking(true);
        while st().threads.contains(&thread) {
            st().context_switch(caller);
        }
    }

    rsched_gunlock();
    st().task_provider.join(thread, retval)
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
    rsched_glock();
    let caller = my_pt();
    st().set_event(
        caller,
        Event {
            instr_addr: return_address(),
            kind: EventKind::LockAcq {
                lock: lock as *const _,
            },
        },
    );
    let tid = st().info[&caller].tid;
    if !st().will_block(key, tid) {
        st().context_switch(caller);
    }
    st().mutex_lock(key, caller);
    // Inform TSAN that this thread has logically acquired the user mutex.
    // Called while GMTX is still held so the annotation is ordered relative
    // to the paired tsan_release in rsched_pthread_mutex_unlock.
    tsan::user_acquire(lock as *mut libc::c_void);
    st().clear_event(caller);
    rsched_gunlock();
    0
}

// ── rsched_pthread_mutex_trylock ──────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_trylock(lock: *mut MutexT) -> libc::c_int {
    ensure_init();
    let key = lock as usize;
    rsched_glock();
    let caller = my_pt();
    st().set_event(
        caller,
        Event {
            instr_addr: return_address(),
            kind: EventKind::LockAcq {
                lock: lock as *const _,
            },
        },
    );
    st().context_switch(caller);
    let tid = st().info[&caller].tid;
    if let Some(m) = st().mutexes.get(&key)
        && m.owner_tid == tid
    {
        if st().is_recursive_mutex(key) {
            st().mutex_lock(key, caller);
            tsan::user_acquire(lock as *mut libc::c_void);
            st().clear_event(caller);
            rsched_gunlock();
            return 0;
        }
        st().clear_event(caller);
        rsched_gunlock();
        return libc::EBUSY;
    }
    if st().will_block(key, tid) {
        st().clear_event(caller);
        rsched_gunlock();
        return libc::EBUSY;
    }
    st().mutex_lock(key, caller);
    tsan::user_acquire(lock as *mut libc::c_void);
    st().clear_event(caller);
    rsched_gunlock();
    0
}

// ── rsched_pthread_mutex_unlock ───────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_mutex_unlock(lock: *mut MutexT) -> libc::c_int {
    ensure_init();
    let key = lock as usize;
    rsched_glock();
    let caller = my_pt();
    st().set_event(
        caller,
        Event {
            instr_addr: return_address(),
            kind: EventKind::LockRel {
                lock: lock as *const _,
            },
        },
    );
    // Unlock before context_switch so that when context_switch runs choose(),
    // the waiter's is_blocking is already false and it appears eligible.
    // tsan_release is called first so TSAN records the release edge before
    // any subsequent tsan_acquire in the new owner.
    tsan::user_release(lock as *mut libc::c_void);
    let r = st().mutex_unlock(key, caller);
    st().context_switch(caller);
    st().clear_event(caller);
    rsched_gunlock();
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
    rsched_glock();
    let caller = my_pt();
    let tid = st().info[&caller].tid;
    if !st().will_block(lkey, tid) {
        st().context_switch(caller);
    }
    let r = st().mutex_unlock(lkey, caller);
    if r != 0 {
        rsched_gunlock();
        return r;
    }
    st().conds
        .entry(ckey)
        .or_insert(SCond {
            waiters: Vec::new(),
        })
        .waiters
        .push(caller);
    st().task(caller).set_blocking(true);
    st().context_switch(caller);
    st().park_if_blocked(caller);

    st().mutex_lock(lkey, caller);
    rsched_gunlock();
    0
}

// ── rsched_pthread_cond_signal ────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_signal(cond: *mut CondT) -> libc::c_int {
    ensure_init();
    let key = cond as usize;
    rsched_glock();
    let caller = my_pt();
    // Use an immutable borrow scoped to a block to get the waiter count,
    // then release it before calling the scheduler.
    let n = { st().conds.get(&key).map_or(0, |c| c.waiters.len()) };
    if n > 0 {
        let blocking = vec![false; n];
        let idx = st()
            .choose_index(&blocking)
            .expect("rsched: non-empty cond waiter list");
        let w = st().conds.get_mut(&key).unwrap().waiters.remove(idx);
        st().task(w).set_blocking(false);
    }
    st().context_switch(caller);
    rsched_gunlock();
    0
}

// ── rsched_pthread_cond_broadcast ─────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_cond_broadcast(cond: *mut CondT) -> libc::c_int {
    ensure_init();
    let key = cond as usize;
    rsched_glock();
    let caller = my_pt();
    if let Some(c) = st().conds.remove(&key) {
        for w in c.waiters {
            st().task(w).set_blocking(false);
        }
    }
    st().context_switch(caller);
    rsched_gunlock();
    0
}

// ── rsched_pthread_barrier_init ───────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_barrier_init(
    barrier: *mut BarrierT,
    attr: *const libc::pthread_barrierattr_t,
    count: libc::c_uint,
) -> libc::c_int {
    ensure_init();
    let key = barrier as usize;
    rsched_glock();
    let caller = my_pt();
    st().context_switch(caller);
    let _ = attr;
    st().barriers.insert(
        key,
        SBarrier {
            count,
            waiters: Vec::new(),
        },
    );
    rsched_gunlock();
    0
}

// ── rsched_pthread_barrier_wait ───────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_pthread_barrier_wait(barrier: *mut BarrierT) -> libc::c_int {
    ensure_init();
    let key = barrier as usize;
    rsched_glock();
    let caller = my_pt();
    st().drain_pending_child();

    // Register as a waiter.
    {
        let b = match st().barriers.get_mut(&key) {
            Some(b) => b,
            None => {
                rsched_gunlock();
                return libc::EINVAL;
            }
        };
        if !b.waiters.contains(&caller) {
            b.waiters.push(caller);
        }
        if b.waiters.len() < b.count as usize {
            st().task(caller).set_blocking(true);
        }
    }

    // If we are the Nth thread, release all waiters.
    let mut released = false;
    {
        let b = st().barriers.get_mut(&key).unwrap();
        if b.waiters.len() >= b.count as usize {
            let ws: Vec<PthreadT> = b.waiters.drain(..).collect();
            for w in ws {
                st().task(w).set_blocking(false);
            }
            released = true;
        }
    }

    // If the barrier is not full yet, this caller must remain blocked until
    // the releasing thread marks all waiters runnable.  If this caller released
    // the barrier, yielding gives the waiters a chance to run.
    st().context_switch(caller);
    st().park_if_blocked(caller);
    rsched_gunlock();
    if released {
        libc::PTHREAD_BARRIER_SERIAL_THREAD
    } else {
        0
    }
}

// ── rsched_sched_yield ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_sched_yield() -> libc::c_int {
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    st().set_event(
        caller,
        Event {
            instr_addr: return_address(),
            kind: EventKind::SchedYield,
        },
    );
    st().context_switch(caller);
    st().clear_event(caller);
    rsched_gunlock();
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

use std::sync::atomic::AtomicUsize;

/// Internal scheduling point for atomic memory operations.
/// `instr_addr` must be obtained via `return_address()` at the call site of the
/// exported atomic function so that it points into user code, not into rsched.
unsafe fn schedule_memop(
    instr_addr: u64,
    mem_addr: *const libc::c_void,
    size: usize,
    access: AccessKind,
) {
    if !instrumented_libc_ready() || is_in_rsched() {
        return;
    }
    ensure_init();
    rsched_glock();
    let caller = my_pt();
    st().set_event(
        caller,
        Event {
            instr_addr,
            kind: EventKind::MemOp {
                mem_addr,
                size,
                access,
            },
        },
    );
    st().context_switch(caller);
    st().clear_event(caller);
    rsched_gunlock();
}

/// Generic hook used by the LLVM pass. It creates a scheduling point for an
/// atomic operation while leaving the original LLVM atomic instruction in place.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_instrument_ra(
    instr_addr: *const libc::c_void,
    ptr: *const libc::c_void,
    size: usize,
    access: libc::c_uint,
) {
    let access = match access {
        0 => AccessKind::Read,
        1 => AccessKind::Write,
        _ => AccessKind::ReadWrite,
    };
    schedule_memop(instr_addr as u64, ptr, size, access);
}
/// Generic hook used by the LLVM pass. It creates a scheduling point for an
/// atomic operation while leaving the original LLVM atomic instruction in place.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_instrument(
    ptr: *const libc::c_void,
    size: usize,
    access: libc::c_uint,
) {
    let access = match access {
        0 => AccessKind::Read,
        1 => AccessKind::Write,
        _ => AccessKind::ReadWrite,
    };
    schedule_memop(return_address(), ptr, size, access);
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
    schedule_memop(
        return_address(),
        ptr,
        std::mem::size_of::<usize>(),
        AccessKind::Read,
    );
    let atomic = &*(ptr as *const AtomicUsize);
    atomic.load(Ordering::SeqCst) as *mut libc::c_void
}

/// Store to an atomic pointer variable.
/// `ptr` is a type-erased pointer to any `T * _Atomic` variable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_atomic_store_ptr(ptr: *mut libc::c_void, val: *mut libc::c_void) {
    schedule_memop(
        return_address(),
        ptr,
        std::mem::size_of::<usize>(),
        AccessKind::Write,
    );
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
