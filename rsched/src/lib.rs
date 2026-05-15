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
use std::collections::{HashMap, HashSet};
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::arch::global_asm;

mod scheduler;
use scheduler::{LoggingScheduler, RandomWalk, Scheduler};

mod event;
pub use event::{AccessKind, Event, EventKind};

// ── Type aliases ──────────────────────────────────────────────────────────

type PthreadT = libc::pthread_t;
type MutexT = libc::pthread_mutex_t;
type CondT = libc::pthread_cond_t;
type BarrierT = libc::pthread_barrier_t;
type AttrT = libc::pthread_attr_t;
type MutexAttrT = libc::pthread_mutexattr_t;
type StartRoutine = unsafe extern "C" fn(*mut libc::c_void) -> *mut libc::c_void;

const MAX_PROCESSES: usize = 32;
const MAX_TASKS_PER_PROCESS: usize = 64;
const MAX_TASKS: usize = MAX_PROCESSES * MAX_TASKS_PER_PROCESS;

// ── Thread descriptor ─────────────────────────────────────────────────────

struct Thread {
    task_id: usize,
    tid: libc::pid_t,
    pthread: PthreadT,
    is_blocking: bool,
    /// False until the parent's rsched_pthread_create returns and the sanitizer's
    /// PostCreate (or equivalent) has been called for this thread.  While false,
    /// context_switch never wakes this thread, preventing it from running
    /// asan_thread_start / tsan_thread_start before the parent has finished the
    /// thread-creation handshake.  The main thread is born with startup_done=true.
    startup_done: bool,
    /// True while this thread is blocked inside rsched's cond_wait (suspend_cond).
    /// External threads (e.g. TSAN's background timer) are never in rsched's
    /// cond_wait after startup, so this flag prevents choose() from picking them
    /// and sending a lost cond_signal that would deadlock the scheduler.
    is_in_rsched_wait: bool,
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
            task_id: usize::MAX,
            tid,
            pthread: pt,
            is_blocking: false,
            startup_done: false,
            is_in_rsched_wait: false,
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

#[derive(Clone, Copy)]
struct TaskChoice {
    task_id: usize,
    process_slot: i32,
    pthread: PthreadT,
}

// ── Scheduler state ───────────────────────────────────────────────────────

struct State {
    scheduler: Box<dyn Scheduler>,
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
        let logging = std::env::var("RSCHED_LOG").is_ok_and(|v| v == "1");
        let scheduler: Box<dyn Scheduler> = if logging {
            Box::new(LoggingScheduler::new(RandomWalk::new(seed)))
        } else {
            Box::new(RandomWalk::new(seed))
        };
        State {
            scheduler,
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
        t.task_id = register_task(pt);
        self.threads.push(pt);
        self.info.insert(pt, Box::new(t));
    }

    fn remove(&mut self, pt: PthreadT) {
        self.threads.retain(|&x| x != pt);
        if let Some(t) = self.info.get_mut(&pt) {
            t.is_blocking = true;
            t.is_exited = true;
            unsafe {
                update_task_status(
                    t.task_id,
                    t.is_blocking,
                    t.startup_done,
                    t.is_in_rsched_wait,
                );
            }
        }
    }

    fn t(&mut self, pt: PthreadT) -> &mut Thread {
        self.info
            .get_mut(&pt)
            .expect("rsched: unknown thread")
            .as_mut()
    }

    unsafe fn refresh_task(&mut self, pt: PthreadT) {
        if let Some(t) = self.info.get(&pt) {
            update_task_status(
                t.task_id,
                t.is_blocking,
                t.startup_done,
                t.is_in_rsched_wait,
            );
        }
    }

    fn choose_index(&mut self, blocking: &[bool]) -> Option<usize> {
        self.scheduler.choose(blocking)
    }

    unsafe fn publish_local_tasks(&mut self, caller: PthreadT) {
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() {
            return;
        }
        process_lock(ps);
        for pt in self.threads.iter().copied() {
            let t = self.info[&pt].as_ref();
            let is_waiting = pt == caller || t.is_in_rsched_wait;
            set_task_status_locked(ps, t.task_id, t.is_blocking, t.startup_done, is_waiting);
        }
        process_unlock(ps);
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
                rsched_cond_wait(addr_of_mut!((*sa).ready_cond));
            }
            drop(Box::from_raw(sa));
            if self.info.contains_key(&child_pt) {
                self.t(child_pt).startup_done = true;
                self.refresh_task(child_pt);
            }
        }
    }

    /// Signal `next`'s suspend_cond and, if `suspend_caller` and next≠caller,
    /// suspend `caller` by waiting on its own cond (releases GMTX atomically).
    /// Must be called with GMTX held.
    unsafe fn wake(&mut self, next: PthreadT, suspend_caller: bool, caller: PthreadT) {
        if next != caller {
            let cond_ptr = addr_of_mut!(self.info.get_mut(&next).unwrap().suspend_cond);
            with_internal_depth(|| (rpt().cond_signal)(cond_ptr));
            if suspend_caller {
                let my_cond = addr_of_mut!(self.info.get_mut(&caller).unwrap().suspend_cond);
                // Mark caller as waiting before releasing GMTX so choose() can see it.
                self.info.get_mut(&caller).unwrap().is_in_rsched_wait = true;
                self.refresh_task(caller);
                rsched_cond_wait(my_cond);
                self.info.get_mut(&caller).unwrap().is_in_rsched_wait = false;
                self.refresh_task(caller);
            }
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

        if !self.info[&caller].startup_done {
            self.t(caller).startup_done = true;
            self.refresh_task(caller);
        }

        if let Some(choice) = self.choose_task(caller) {
            if choice.pthread != caller
                || choice.process_slot != PROCESS_SLOT.load(Ordering::Acquire)
            {
                self.run_task(choice, caller);
            }
        }

        let event = self.info.get(&caller).and_then(|t| t.next_event);
        self.scheduler.on_event(event.as_ref());
    }

    unsafe fn choose_task(&mut self, caller: PthreadT) -> Option<TaskChoice> {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() {
            let mut tasks = Vec::new();
            for pt in self.threads.iter().copied() {
                let t = self.info[&pt].as_ref();
                if !t.is_blocking && t.startup_done && (pt == caller || t.is_in_rsched_wait) {
                    tasks.push(TaskChoice {
                        task_id: t.task_id,
                        process_slot: current,
                        pthread: pt,
                    });
                }
            }
            let blocking = vec![false; tasks.len()];
            return self.choose_index(&blocking).map(|idx| tasks[idx]);
        }

        self.publish_local_tasks(caller);

        let mut tasks = [TaskChoice {
            task_id: usize::MAX,
            process_slot: -1,
            pthread: 0 as PthreadT,
        }; MAX_TASKS];
        let mut n = 0usize;
        process_lock(ps);
        for id in 0..(*ps).task_count.min(MAX_TASKS) {
            let task = (*ps).tasks[id];
            if task.active != 0
                && task.process_slot >= 0
                && (*ps).slots[task.process_slot as usize].active != 0
                && task.is_blocking == 0
                && task.startup_done != 0
                && task.is_waiting != 0
            {
                tasks[n] = TaskChoice {
                    task_id: id,
                    process_slot: task.process_slot,
                    pthread: task.pthread as PthreadT,
                };
                n += 1;
            }
        }
        let blocking = vec![false; n];
        let choice = self.choose_index(&blocking).map(|idx| tasks[idx]);
        process_unlock(ps);
        choice
    }

    unsafe fn run_task(&mut self, choice: TaskChoice, caller: PthreadT) {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        if choice.process_slot == current {
            self.wake(choice.pthread, true, caller);
            return;
        }

        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() {
            return;
        }

        process_lock(ps);
        if choice.process_slot < 0 || (*ps).slots[choice.process_slot as usize].active == 0 {
            process_unlock(ps);
            return;
        }
        (*ps).slots[choice.process_slot as usize].selected_task = choice.task_id;
        process_log(format_args!(
            "pid {} switch slot {} -> {} thread {:#x}",
            libc::getpid(),
            current,
            choice.process_slot,
            choice.pthread as usize
        ));
        let r = with_internal_depth(|| {
            libc::sem_post(addr_of_mut!((*ps).slots[choice.process_slot as usize].gate))
        });
        assert_eq!(r, 0, "rsched: sem_post failed");
        process_unlock(ps);

        self.info.get_mut(&caller).unwrap().is_in_rsched_wait = true;
        self.refresh_task(caller);
        process_wait_on_slot(ps, current);
        self.resume_selected_thread(caller);
    }

    unsafe fn resume_selected_thread(&mut self, caller: PthreadT) {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if current < 0 || ps.is_null() {
            return;
        }
        process_lock(ps);
        let selected_task = (*ps).slots[current as usize].selected_task;
        (*ps).slots[current as usize].selected_task = usize::MAX;
        let selected = if selected_task < (*ps).task_count {
            (*ps).tasks[selected_task].pthread as PthreadT
        } else {
            0 as PthreadT
        };
        process_unlock(ps);
        process_log(format_args!(
            "pid {} woke slot {} selected task {} thread {:#x} caller {:#x}",
            libc::getpid(),
            current,
            selected_task,
            selected as usize,
            caller as usize
        ));

        if selected != 0 as PthreadT && self.info.contains_key(&selected) && selected != caller {
            self.wake(selected, true, caller);
        } else if self.info.contains_key(&caller) {
            self.t(caller).is_in_rsched_wait = false;
            self.refresh_task(caller);
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
            self.t(caller).is_blocking = true;
            self.context_switch(caller);
            // context_switch may have returned immediately (None path) without
            // suspending us, e.g. when the mutex owner is running but not in
            // rsched's cond_wait (common in TSAN mode where threads run freely).
            // In that case we must release GMTX by doing a real cond_wait so
            // the owner can acquire GMTX to call mutex_unlock.
            // mutex_unlock will signal our suspend_cond after transferring
            // ownership, so this wait is always eventually resolved.
            if self.t(caller).is_blocking {
                self.t(caller).is_in_rsched_wait = true;
                rsched_cond_wait(addr_of_mut!(self.t(caller).suspend_cond));
                self.t(caller).is_in_rsched_wait = false;
            }
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
        self.t(waiter).is_blocking = false;
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

// ── Globals ───────────────────────────────────────────────────────────────

static mut GMTX: MutexT = libc::PTHREAD_MUTEX_INITIALIZER;
static mut STATE: Option<State> = None;
static INITED: AtomicBool = AtomicBool::new(false);
static PROCESS_SHARED: AtomicPtr<ProcessShared> = AtomicPtr::new(core::ptr::null_mut());
static PROCESS_SLOT: AtomicI32 = AtomicI32::new(-1);
static FORK_SLOT: AtomicI32 = AtomicI32::new(-1);

thread_local! {
    static MY_PT: RefCell<PthreadT> = const { RefCell::new(unsafe { std::mem::zeroed() }) };
}

fn my_pt() -> PthreadT {
    MY_PT.with(|c| *c.borrow())
}

#[repr(C)]
struct ProcessSlot {
    pid: libc::pid_t,
    active: u8,
    _pad: [u8; 3],
    selected_task: usize,
    gate: libc::sem_t,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SharedTask {
    active: u8,
    is_blocking: u8,
    startup_done: u8,
    is_waiting: u8,
    process_slot: i32,
    pthread: usize,
}

#[repr(C)]
struct ProcessShared {
    lock: MutexT,
    task_count: usize,
    tasks: [SharedTask; MAX_TASKS],
    slots: [ProcessSlot; MAX_PROCESSES],
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
            barrier_init: sym(lib, b"pthread_barrier_init\0"),
        }
    })
}

unsafe fn glock() {
    with_internal_depth(|| (rpt().mutex_lock)(addr_of_mut!(GMTX)));
}
unsafe fn gunlock() {
    with_internal_depth(|| (rpt().mutex_unlock)(addr_of_mut!(GMTX)));
}

unsafe fn sync_acquire() {
    tsan_ignore_begin();
}

unsafe fn sync_release() {
    tsan_ignore_end();
}

unsafe fn rsched_glock() {
    glock();
    sync_acquire();
}

unsafe fn rsched_gunlock() {
    sync_release();
    gunlock();
}

unsafe fn rsched_cond_wait(cond: *mut CondT) {
    sync_release();
    with_internal_depth(|| (rpt().cond_wait)(cond, addr_of_mut!(GMTX)));
    sync_acquire();
}

unsafe fn process_shared_ptr() -> *mut ProcessShared {
    let mut p = PROCESS_SHARED.load(Ordering::Acquire);
    if !p.is_null() {
        return p;
    }

    let size = core::mem::size_of::<ProcessShared>();
    let raw = libc::mmap(
        core::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANONYMOUS | libc::MAP_SHARED,
        -1,
        0,
    );
    assert_ne!(
        raw,
        libc::MAP_FAILED,
        "rsched: mmap shared process state failed"
    );
    core::ptr::write_bytes(raw, 0, size);
    p = raw.cast::<ProcessShared>();

    let mut attr: libc::pthread_mutexattr_t = core::mem::zeroed();
    let r = with_internal_depth(|| libc::pthread_mutexattr_init(&mut attr));
    assert_eq!(r, 0, "rsched: pthread_mutexattr_init failed");
    let r = with_internal_depth(|| {
        libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED)
    });
    assert_eq!(r, 0, "rsched: pthread_mutexattr_setpshared failed");
    let r = with_internal_depth(|| libc::pthread_mutex_init(addr_of_mut!((*p).lock), &attr));
    assert_eq!(r, 0, "rsched: process-shared mutex init failed");
    with_internal_depth(|| libc::pthread_mutexattr_destroy(&mut attr));

    match PROCESS_SHARED.compare_exchange(
        core::ptr::null_mut(),
        p,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => p,
        Err(existing) => {
            libc::munmap(raw, size);
            existing
        }
    }
}

fn process_log(args: core::fmt::Arguments<'_>) {
    if std::env::var("RSCHED_PROCESS_LOG").is_ok_and(|v| v == "1") {
        eprintln!("[rsched-process] {args}");
    }
}

unsafe fn process_lock(ps: *mut ProcessShared) {
    let r = with_internal_depth(|| (rpt().mutex_lock)(addr_of_mut!((*ps).lock)));
    assert_eq!(r, 0, "rsched: process shared lock failed");
}

unsafe fn process_unlock(ps: *mut ProcessShared) {
    let r = with_internal_depth(|| (rpt().mutex_unlock)(addr_of_mut!((*ps).lock)));
    assert_eq!(r, 0, "rsched: process shared unlock failed");
}

unsafe fn set_task_status_locked(
    ps: *mut ProcessShared,
    task_id: usize,
    is_blocking: bool,
    startup_done: bool,
    is_waiting: bool,
) {
    if task_id >= (*ps).task_count || task_id >= MAX_TASKS {
        return;
    }
    let task = &mut (*ps).tasks[task_id];
    task.is_blocking = u8::from(is_blocking);
    task.startup_done = u8::from(startup_done);
    task.is_waiting = u8::from(is_waiting);
}

unsafe fn update_task_status(
    task_id: usize,
    is_blocking: bool,
    startup_done: bool,
    is_waiting: bool,
) {
    let ps = PROCESS_SHARED.load(Ordering::Acquire);
    if ps.is_null() || task_id == usize::MAX {
        return;
    }
    process_lock(ps);
    set_task_status_locked(ps, task_id, is_blocking, startup_done, is_waiting);
    process_unlock(ps);
}

unsafe fn register_task(pt: PthreadT) -> usize {
    let slot = PROCESS_SLOT.load(Ordering::Acquire);
    let ps = process_shared_ptr();
    process_lock(ps);
    if (*ps).task_count >= MAX_TASKS {
        process_unlock(ps);
        panic!("rsched: too many tasks; increase MAX_TASKS");
    }
    let task_id = (*ps).task_count;
    (*ps).task_count += 1;
    (*ps).tasks[task_id] = SharedTask {
        active: 1,
        is_blocking: 0,
        startup_done: 0,
        is_waiting: 0,
        process_slot: slot,
        pthread: pt as usize,
    };
    process_unlock(ps);
    task_id
}

unsafe fn deactivate_process_tasks_locked(ps: *mut ProcessShared, process_slot: i32) {
    for id in 0..(*ps).task_count.min(MAX_TASKS) {
        if (*ps).tasks[id].process_slot == process_slot {
            (*ps).tasks[id].active = 0;
            (*ps).tasks[id].is_blocking = 1;
            (*ps).tasks[id].is_waiting = 0;
        }
    }
}

unsafe fn process_register_current(initially_runnable: bool) -> i32 {
    let ps = process_shared_ptr();
    process_lock(ps);
    let pid = libc::getpid();

    for i in 0..MAX_PROCESSES {
        if (*ps).slots[i].active != 0 && (*ps).slots[i].pid == pid {
            process_unlock(ps);
            PROCESS_SLOT.store(i as i32, Ordering::Release);
            return i as i32;
        }
    }

    for i in 0..MAX_PROCESSES {
        if (*ps).slots[i].active == 0 {
            let r = with_internal_depth(|| {
                libc::sem_init(
                    addr_of_mut!((*ps).slots[i].gate),
                    1,
                    u32::from(initially_runnable),
                )
            });
            assert_eq!(r, 0, "rsched: sem_init failed");
            (*ps).slots[i].pid = pid;
            (*ps).slots[i].active = 1;
            (*ps).slots[i].selected_task = usize::MAX;
            process_unlock(ps);
            PROCESS_SLOT.store(i as i32, Ordering::Release);
            if initially_runnable {
                let mut discard = 0;
                while with_internal_depth(|| libc::sem_trywait(addr_of_mut!((*ps).slots[i].gate)))
                    == 0
                {
                    discard += 1;
                    if discard > 1 {
                        break;
                    }
                }
            }
            return i as i32;
        }
    }

    process_unlock(ps);
    panic!("rsched: too many forked processes; increase MAX_PROCESSES");
}

unsafe fn process_wait_on_slot(ps: *mut ProcessShared, slot: i32) {
    loop {
        let r =
            with_internal_depth(|| libc::sem_wait(addr_of_mut!((*ps).slots[slot as usize].gate)));
        if r == 0 {
            return;
        }
        let errno = *libc::__errno_location();
        assert_eq!(errno, libc::EINTR, "rsched: sem_wait failed");
    }
}

unsafe fn process_wait_until_published(ps: *mut ProcessShared, slot: i32) {
    loop {
        process_lock(ps);
        let mut ready = false;
        if slot >= 0 && (*ps).slots[slot as usize].active != 0 {
            for id in 0..(*ps).task_count.min(MAX_TASKS) {
                let task = (*ps).tasks[id];
                if task.active != 0 && task.process_slot == slot {
                    ready = true;
                    break;
                }
            }
        }
        process_unlock(ps);
        if ready {
            return;
        }
        with_internal_depth(|| libc::sched_yield());
    }
}

extern "C" fn process_atexit() {
    unsafe {
        rsched_process_exit();
    }
}

unsafe fn reset_local_state_after_fork() {
    STATE = None;
    INITED.store(false, Ordering::SeqCst);
    rsched_init();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_before_fork() {
    ensure_init();
    let ps = process_shared_ptr();
    process_lock(ps);
    for i in 0..MAX_PROCESSES {
        if (*ps).slots[i].active == 0 {
            let r = with_internal_depth(|| libc::sem_init(addr_of_mut!((*ps).slots[i].gate), 1, 0));
            assert_eq!(r, 0, "rsched: fork sem_init failed");
            (*ps).slots[i].pid = 0;
            (*ps).slots[i].active = 1;
            (*ps).slots[i].selected_task = usize::MAX;
            FORK_SLOT.store(i as i32, Ordering::Release);
            process_log(format_args!(
                "pid {} reserved fork slot {}",
                libc::getpid(),
                i
            ));
            process_unlock(ps);
            return;
        }
    }
    process_unlock(ps);
    panic!("rsched: too many forked processes; increase MAX_PROCESSES");
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_after_fork_parent(child: libc::pid_t) {
    let slot = FORK_SLOT.swap(-1, Ordering::AcqRel);
    if slot < 0 {
        return;
    }
    let ps = process_shared_ptr();
    process_lock(ps);
    if child <= 0 {
        (*ps).slots[slot as usize].active = 0;
        process_unlock(ps);
        return;
    }
    (*ps).slots[slot as usize].pid = child;
    process_log(format_args!(
        "pid {} parent registered child {} in slot {}",
        libc::getpid(),
        child,
        slot
    ));
    process_unlock(ps);
    process_wait_until_published(ps, slot);
    rsched_glock();
    let caller = my_pt();
    st().context_switch(caller);
    rsched_gunlock();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_after_fork_child() {
    let slot = FORK_SLOT.swap(-1, Ordering::AcqRel);
    let ps = process_shared_ptr();
    if slot >= 0 {
        process_lock(ps);
        (*ps).slots[slot as usize].pid = libc::getpid();
        process_log(format_args!(
            "pid {} child using fork slot {}",
            libc::getpid(),
            slot
        ));
        process_unlock(ps);
        PROCESS_SLOT.store(slot, Ordering::Release);
    } else {
        process_register_current(false);
    }
    reset_local_state_after_fork();
    rsched_glock();
    st().publish_local_tasks(my_pt());
    rsched_gunlock();
    process_log(format_args!(
        "pid {} child waiting on slot {}",
        libc::getpid(),
        PROCESS_SLOT.load(Ordering::Acquire)
    ));
    process_wait_on_slot(ps, PROCESS_SLOT.load(Ordering::Acquire));
    process_lock(ps);
    (*ps).slots[PROCESS_SLOT.load(Ordering::Acquire) as usize].selected_task = usize::MAX;
    process_unlock(ps);
    process_log(format_args!("pid {} child resumed", libc::getpid()));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_process_exit() {
    let current = PROCESS_SLOT.swap(-1, Ordering::AcqRel);
    if current < 0 {
        return;
    }
    let ps = PROCESS_SHARED.load(Ordering::Acquire);
    if ps.is_null() {
        return;
    }
    if INITED.load(Ordering::Acquire) {
        rsched_glock();
    }
    process_lock(ps);
    (*ps).slots[current as usize].active = 0;
    deactivate_process_tasks_locked(ps, current);
    process_unlock(ps);
    let next = if INITED.load(Ordering::Acquire) {
        st().choose_task(0 as PthreadT)
    } else {
        None
    };
    process_log(format_args!(
        "pid {} exit slot {}, next slot {} thread {:#x}",
        libc::getpid(),
        current,
        next.map_or(-1, |choice| choice.process_slot),
        next.map_or(0, |choice| choice.pthread as usize)
    ));
    if let Some(choice) = next
        && choice.process_slot != current
        && choice.process_slot >= 0
    {
        process_lock(ps);
        (*ps).slots[choice.process_slot as usize].selected_task = choice.task_id;
        let _ = with_internal_depth(|| {
            libc::sem_post(addr_of_mut!((*ps).slots[choice.process_slot as usize].gate))
        });
        process_unlock(ps);
    }
    if INITED.load(Ordering::Acquire) {
        rsched_gunlock();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fork() -> libc::pid_t {
    ensure_init();
    rsched_before_fork();
    let pid = with_internal_depth(|| libc::fork());
    if pid == 0 {
        rsched_after_fork_child();
    } else {
        rsched_after_fork_parent(pid);
    }
    pid
}

unsafe fn tsan_user_acquire(addr: *mut libc::c_void) {
    tsan_ignore_end();
    tsan_acquire(addr);
    tsan_ignore_begin();
}

unsafe fn tsan_user_release(addr: *mut libc::c_void) {
    tsan_ignore_end();
    tsan_release(addr);
    tsan_ignore_begin();
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

/// Increment depth; return true iff this is the outermost (non-reentrant) call.
/// Called by the preload interceptors on every entry.
pub fn rsched_try_enter() -> bool {
    CALL_DEPTH.with(|d| d.fetch_add(1, Ordering::Acquire) == 0)
}

/// Decrement depth. Called by the preload interceptors on every exit.
pub fn rsched_exit() {
    CALL_DEPTH.with(|d| {
        d.fetch_sub(1, Ordering::Release);
    });
}

/// Increment depth without checking. Used internally by trampoline/do_thread_exit.
#[inline]
fn depth_enter() {
    CALL_DEPTH.with(|d| {
        d.fetch_add(1, Ordering::Acquire);
    });
}

/// Decrement depth. Paired with depth_enter().
#[inline]
fn depth_exit() {
    CALL_DEPTH.with(|d| {
        d.fetch_sub(1, Ordering::Release);
    });
}

#[inline]
fn is_in_rsched() -> bool {
    CALL_DEPTH.with(|d| d.load(Ordering::Relaxed) > 0)
}

#[inline]
unsafe fn with_internal_depth<T>(f: impl FnOnce() -> T) -> T {
    depth_enter();
    let r = f();
    depth_exit();
    r
}

// ── Optional seccomp tripwire ────────────────────────────────────────────────
//
// RSCHED_SECCOMP=1 installs a process-wide filter that traps raw futex syscalls
// unless they come from rsched's single internal raw-syscall instruction.  The
// SIGSYS handler either emulates the syscall through that whitelisted
// instruction while rsched is active, or aborts if user code reached such a
// syscall without going through an intercepted pthread/synchronization API.
//
// RSCHED_SECCOMP_TRAP_CLONE=1 also traps clone/clone3.  That is intentionally
// not part of RSCHED_SECCOMP=1 because clone cannot be safely emulated from a
// SIGSYS handler: the child returns on its new stack without the signal frame
// needed to resume the original trapped instruction.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
global_asm!(
    r#"
    .text
    .globl rsched_internal_syscall6
    .hidden rsched_internal_syscall6
    .type rsched_internal_syscall6,@function
rsched_internal_syscall6:
    mov rax, rdi
    mov rdi, rsi
    mov rsi, rdx
    mov rdx, rcx
    mov r10, r8
    mov r8,  r9
    mov r9,  qword ptr [rsp + 8]
    .globl rsched_internal_syscall6_insn
    .hidden rsched_internal_syscall6_insn
rsched_internal_syscall6_insn:
    syscall
    ret
    .size rsched_internal_syscall6, .-rsched_internal_syscall6
"#
);

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe extern "C" {
    fn rsched_internal_syscall6(
        nr: libc::c_long,
        a0: libc::c_long,
        a1: libc::c_long,
        a2: libc::c_long,
        a3: libc::c_long,
        a4: libc::c_long,
        a5: libc::c_long,
    ) -> libc::c_long;

    static rsched_internal_syscall6_insn: u8;
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn raw_syscall6(
    nr: libc::c_long,
    a0: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
) -> libc::c_long {
    rsched_internal_syscall6(nr, a0, a1, a2, a3, a4, a5)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
extern "C" fn seccomp_sigsys_handler(
    _sig: libc::c_int,
    _info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    unsafe {
        let ctx = &mut *(ucontext as *mut libc::ucontext_t);
        let regs = &mut ctx.uc_mcontext.gregs;
        let nr = regs[libc::REG_RAX as usize] as libc::c_long;

        if is_in_rsched() {
            let ret = raw_syscall6(
                nr,
                regs[libc::REG_RDI as usize] as libc::c_long,
                regs[libc::REG_RSI as usize] as libc::c_long,
                regs[libc::REG_RDX as usize] as libc::c_long,
                regs[libc::REG_R10 as usize] as libc::c_long,
                regs[libc::REG_R8 as usize] as libc::c_long,
                regs[libc::REG_R9 as usize] as libc::c_long,
            );
            regs[libc::REG_RAX as usize] = ret;
            return;
        }

        const MSG: &[u8] = b"rsched: intercepted raw futex/clone syscall outside rsched\n";
        let _ = raw_syscall6(
            libc::SYS_write as libc::c_long,
            libc::STDERR_FILENO as libc::c_long,
            MSG.as_ptr() as libc::c_long,
            MSG.len() as libc::c_long,
            0,
            0,
            0,
        );
        let _ = raw_syscall6(libc::SYS_getpid as libc::c_long, 0, 0, 0, 0, 0, 0);
        let _ = raw_syscall6(libc::SYS_exit_group as libc::c_long, 101, 0, 0, 0, 0, 0);
        core::hint::unreachable_unchecked();
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn install_seccomp_tripwire_if_requested() {
    if std::env::var("RSCHED_SECCOMP").map_or(true, |v| v != "1") {
        return;
    }

    install_sigsys_handler();
    install_seccomp_filter();
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
unsafe fn install_seccomp_tripwire_if_requested() {
    if std::env::var("RSCHED_SECCOMP").map_or(false, |v| v == "1") {
        panic!("rsched: RSCHED_SECCOMP=1 is only implemented on linux x86_64");
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn install_sigsys_handler() {
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_sigaction = seccomp_sigsys_handler as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    libc::sigemptyset(&mut sa.sa_mask);
    let r = libc::sigaction(libc::SIGSYS, &sa, core::ptr::null_mut());
    assert_eq!(r, 0, "rsched: sigaction(SIGSYS) failed");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn install_seccomp_filter() {
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const SECCOMP_DATA_NR: u32 = 0;
    const SECCOMP_DATA_ARCH: u32 = 4;
    const SECCOMP_DATA_IP_LO: u32 = 8;
    const SECCOMP_DATA_IP_HI: u32 = 12;
    const SYS_CLONE3_X86_64: u32 = 435;

    fn stmt(code: u16, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }
    fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }

    let syscall_ip = &rsched_internal_syscall6_insn as *const u8 as usize as u64 + 2;
    let syscall_ip_lo = syscall_ip as u32;
    let syscall_ip_hi = (syscall_ip >> 32) as u32;
    let trap_clone = std::env::var("RSCHED_SECCOMP_TRAP_CLONE").is_ok_and(|v| v == "1");
    let clone_syscall = if trap_clone {
        libc::SYS_clone as u32
    } else {
        u32::MAX
    };
    let clone3_syscall = if trap_clone {
        SYS_CLONE3_X86_64
    } else {
        u32::MAX - 1
    };

    let mut filter = [
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_ARCH,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            AUDIT_ARCH_X86_64,
            1,
            0,
        ),
        stmt(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_KILL_PROCESS,
        ),
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_NR,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            libc::SYS_futex as u32,
            3,
            0,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            clone_syscall,
            2,
            0,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            clone3_syscall,
            1,
            0,
        ),
        stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_ALLOW),
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_IP_LO,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            syscall_ip_lo,
            0,
            2,
        ),
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_IP_HI,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            syscall_ip_hi,
            1,
            0,
        ),
        stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_TRAP),
        stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_ALLOW),
    ];
    let mut prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };

    let r = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    assert_eq!(r, 0, "rsched: prctl(PR_SET_NO_NEW_PRIVS) failed");
    let r = libc::prctl(
        libc::PR_SET_SECCOMP,
        libc::SECCOMP_MODE_FILTER,
        &mut prog as *mut libc::sock_fprog,
    );
    assert_eq!(r, 0, "rsched: prctl(PR_SET_SECCOMP) failed");
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
        #[linkage = "extern_weak"]
        static __tsan_acquire: Option<unsafe extern "C" fn(*mut libc::c_void)>;
    }
    if let Some(f) = __tsan_acquire {
        f(addr);
    }
}

#[cfg(not(feature = "tsan"))]
#[inline(always)]
unsafe fn tsan_acquire(_addr: *mut libc::c_void) {}

#[cfg(feature = "tsan")]
unsafe fn tsan_release(addr: *mut libc::c_void) {
    unsafe extern "C" {
        #[linkage = "extern_weak"]
        static __tsan_release: Option<unsafe extern "C" fn(*mut libc::c_void)>;
    }
    if let Some(f) = __tsan_release {
        f(addr);
    }
}

#[cfg(not(feature = "tsan"))]
#[inline(always)]
unsafe fn tsan_release(_addr: *mut libc::c_void) {}

#[cfg(feature = "tsan")]
unsafe fn tsan_ignore_begin() {
    unsafe extern "C" {
        #[linkage = "extern_weak"]
        static __tsan_ignore_thread_begin: Option<unsafe extern "C" fn()>;
    }
    if let Some(f) = __tsan_ignore_thread_begin {
        f();
    }
}

#[cfg(not(feature = "tsan"))]
#[inline(always)]
unsafe fn tsan_ignore_begin() {}

#[cfg(feature = "tsan")]
unsafe fn tsan_ignore_end() {
    unsafe extern "C" {
        #[linkage = "extern_weak"]
        static __tsan_ignore_thread_end: Option<unsafe extern "C" fn()>;
    }
    if let Some(f) = __tsan_ignore_thread_end {
        f();
    }
}

#[cfg(not(feature = "tsan"))]
#[inline(always)]
unsafe fn tsan_ignore_end() {}

unsafe fn st() -> &'static mut State {
    (*addr_of_mut!(STATE))
        .as_mut()
        .expect("rsched not initialised")
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
    let seed: u64 = std::env::var("RANDOM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x12345678abcdu64);
    STATE = Some(State::new(seed));
    let first_process = PROCESS_SLOT.load(Ordering::Acquire) < 0;
    process_register_current(first_process);
    if first_process {
        with_internal_depth(|| libc::atexit(process_atexit));
    }

    let self_pt = libc::pthread_self();
    MY_PT.with(|c| *c.borrow_mut() = self_pt);
    rsched_glock();
    let mut t = Thread::new(self_pt);
    t.startup_done = true; // main thread needs no sanitizer handshake
    st().add(t);
    st().refresh_task(self_pt);
    rsched_gunlock();
    install_seccomp_tripwire_if_requested();
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
    let current_slot = PROCESS_SLOT.load(Ordering::Acquire);
    let ps = PROCESS_SHARED.load(Ordering::Acquire);
    if current_slot >= 0 && !ps.is_null() {
        process_lock(ps);
        deactivate_process_tasks_locked(ps, current_slot);
        process_unlock(ps);
    }

    // Drop any existing state (previous test run's threads/mutexes/etc.).
    STATE = None;
    STATE = Some(State::new(seed));
    INITED.store(true, Ordering::SeqCst);
    if PROCESS_SLOT.load(Ordering::Acquire) < 0 {
        process_register_current(true);
    }

    let self_pt = libc::pthread_self();
    MY_PT.with(|c| *c.borrow_mut() = self_pt);
    rsched_glock();
    let mut t = Thread::new(self_pt);
    t.startup_done = true;
    st().add(t);
    st().refresh_task(self_pt);
    rsched_gunlock();
}

type FuzzerTestOneInput =
    unsafe extern "C" fn(data: *const libc::c_uchar, size: usize) -> libc::c_int;

fn fuzzer_schedule_count() -> usize {
    std::env::var("RSCHED_FUZZ_SCHEDULES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

fn fuzzer_input_hash(data: *const libc::c_uchar, size: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    if !data.is_null() {
        for byte in unsafe { std::slice::from_raw_parts(data, size) } {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash ^ (size as u64).wrapping_mul(FNV_PRIME)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fuzzer_test_one_input(
    data: *const libc::c_uchar,
    size: usize,
    test_one_input: FuzzerTestOneInput,
) -> libc::c_int {
    let base_seed = fuzzer_input_hash(data, size);
    let mut result = 0;
    for schedule in 0..fuzzer_schedule_count() {
        unsafe {
            rsched_reinit(base_seed.wrapping_add(schedule as u64));
            result = test_one_input(data, size);
        }
    }
    result
}

// ── Thread trampoline ─────────────────────────────────────────────────────

struct StartArg {
    routine: StartRoutine,
    arg: *mut libc::c_void,
    /// Signaled (with GMTX held) once the new thread has registered itself.
    ready_cond: CondT,
    ready: bool,
}
unsafe impl Send for StartArg {}

#[cfg(feature = "tsan")]
struct TSanGateArg {
    routine: StartRoutine,
    arg: *mut libc::c_void,
}

#[cfg(feature = "tsan")]
unsafe impl Send for TSanGateArg {}

#[cfg(feature = "tsan")]
extern "C" fn tsan_user_start_gate(raw: *mut libc::c_void) -> *mut libc::c_void {
    unsafe {
        let gate = Box::from_raw(raw as *mut TSanGateArg);
        let routine = gate.routine;
        let arg = gate.arg;
        let self_pt = my_pt();

        // TSAN has completed its parent/child pthread_create handshake before
        // calling this gate.  Now block under rsched control so the child
        // cannot enter user code until a deterministic scheduler decision
        // wakes it.
        depth_enter();
        rsched_glock();
        if st().info.contains_key(&self_pt) {
            st().t(self_pt).is_in_rsched_wait = true;
            let cond_ptr = addr_of_mut!(st().t(self_pt).suspend_cond);
            rsched_cond_wait(cond_ptr);
            st().t(self_pt).is_in_rsched_wait = false;
        }
        rsched_gunlock();
        depth_exit();

        routine(arg)
    }
}

#[cfg(feature = "tsan")]
const TSAN_THREAD_START_PREFIX: &[u8] = &[
    0x55, 0x41, 0x57, 0x41, 0x56, 0x41, 0x55, 0x41, 0x54, 0x53, 0x50, 0x49, 0x89, 0xfe, 0x4c, 0x8b,
    0x27, 0x48, 0x8b, 0x5f, 0x08,
];

#[cfg(feature = "tsan")]
const TSAN_BACKGROUND_START_PREFIX: &[u8] = &[
    0x64, 0x48, 0x8b, 0x04, 0x25, 0xe8, 0xf8, 0xff, 0xff, 0x48, 0x85, 0xc0, 0x0f, 0x84, 0x8d, 0x03,
    0x00, 0x00, 0x55, 0x41, 0x57, 0x41, 0x56, 0x41, 0x55, 0x41, 0x54, 0x53, 0x48, 0x83, 0xec, 0x18,
];

#[cfg(feature = "tsan")]
pub unsafe fn rsched_is_tsan_thread_start(start: StartRoutine, arg: *mut libc::c_void) -> bool {
    if arg.is_null() {
        return false;
    }

    let code = std::slice::from_raw_parts(start as *const u8, TSAN_THREAD_START_PREFIX.len());
    code == TSAN_THREAD_START_PREFIX
}

#[cfg(feature = "tsan")]
pub unsafe fn rsched_is_tsan_background_start(start: StartRoutine, arg: *mut libc::c_void) -> bool {
    if !arg.is_null() {
        return false;
    }

    let code = std::slice::from_raw_parts(start as *const u8, TSAN_BACKGROUND_START_PREFIX.len());
    code == TSAN_BACKGROUND_START_PREFIX
}

#[cfg(feature = "tsan")]
unsafe fn prepare_tsan_start_gate(arg: *mut libc::c_void) -> *mut TSanGateArg {
    let fields = arg as *mut usize;
    let routine = std::mem::transmute_copy::<usize, StartRoutine>(&*fields);
    let user_arg = *fields.add(1) as *mut libc::c_void;
    let gate = Box::into_raw(Box::new(TSanGateArg {
        routine,
        arg: user_arg,
    }));
    *fields = tsan_user_start_gate as *const () as usize;
    *fields.add(1) = gate as usize;
    gate
}

#[cfg(feature = "tsan")]
unsafe fn restore_tsan_start_gate(arg: *mut libc::c_void, gate: *mut TSanGateArg) {
    let gate_box = Box::from_raw(gate);
    let fields = arg as *mut usize;
    *fields = gate_box.routine as usize;
    *fields.add(1) = gate_box.arg as usize;
}

/// Shared cleanup logic for thread exit.  Must be called with GMTX *not* held.
unsafe fn do_thread_exit(caller: PthreadT) {
    // Raise CALL_DEPTH before acquiring GMTX so that any libc-internal PLT
    // re-entry through our preload interceptors is treated as non-outermost.
    depth_enter();
    rsched_glock();
    let joiner_opt = st().info.get(&caller).and_then(|t| t.joiner);
    if let Some(joiner) = joiner_opt {
        st().t(joiner).is_blocking = false;
        st().refresh_task(joiner);
    }
    st().remove(caller);
    let mut local_tasks = Vec::new();
    for pt in st().threads.iter().copied() {
        let t = st().info[&pt].as_ref();
        if !t.is_blocking && t.startup_done && t.is_in_rsched_wait {
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
        with_internal_depth(|| (rpt().cond_signal)(cond_ptr));
    } else if let Some(choice) = st().choose_task(0 as PthreadT) {
        let current_slot = PROCESS_SLOT.load(Ordering::Acquire);
        if choice.process_slot == current_slot && st().info.contains_key(&choice.pthread) {
            let cond_ptr = addr_of_mut!(st().t(choice.pthread).suspend_cond);
            with_internal_depth(|| (rpt().cond_signal)(cond_ptr));
        } else if choice.process_slot >= 0 {
            let ps = PROCESS_SHARED.load(Ordering::Acquire);
            if !ps.is_null() {
                process_lock(ps);
                (*ps).slots[choice.process_slot as usize].selected_task = choice.task_id;
                let _ = with_internal_depth(|| {
                    libc::sem_post(addr_of_mut!((*ps).slots[choice.process_slot as usize].gate))
                });
                process_unlock(ps);
            }
        }
    }
    rsched_gunlock();
    depth_exit();
}

// Safe fn required because libc::pthread_create takes a safe fn pointer.
extern "C" fn trampoline(raw: *mut libc::c_void) -> *mut libc::c_void {
    unsafe {
        let sa = &mut *(raw as *mut StartArg);
        let routine = sa.routine;
        let arg = sa.arg;

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
        st().t(self_pt).tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;

        // Signal parent that we are registered (parent may or may not be waiting).
        sa.ready = true;
        with_internal_depth(|| (rpt().cond_signal)(addr_of_mut!(sa.ready_cond)));

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
            st().t(self_pt).is_in_rsched_wait = true;
            let cond_ptr = addr_of_mut!(st().t(self_pt).suspend_cond);
            rsched_cond_wait(cond_ptr);
            st().t(self_pt).is_in_rsched_wait = false;
        }

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
    thread: *mut PthreadT,
    attr: *const AttrT,
    start_routine: StartRoutine,
    arg: *mut libc::c_void,
) -> libc::c_int {
    ensure_init();

    #[cfg(feature = "tsan")]
    let tsan_gate = if rsched_is_tsan_thread_start(start_routine, arg) {
        Some(prepare_tsan_start_gate(arg))
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

    // Create the OS thread immediately.
    #[cfg(feature = "asan")]
    let r = with_internal_depth(|| {
        libc::pthread_create(thread, attr, trampoline, sa as *mut libc::c_void)
    });
    #[cfg(not(feature = "asan"))]
    let r =
        with_internal_depth(|| (rpt().create)(thread, attr, trampoline, sa as *mut libc::c_void));

    if r != 0 {
        drop(Box::from_raw(sa));
        #[cfg(feature = "tsan")]
        if let Some(gate) = tsan_gate {
            restore_tsan_start_gate(arg, gate);
        }
        rsched_gunlock();
        return r;
    }

    st().add(Thread::new_with_tid(*thread, 0));

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
        st().t(caller).is_blocking = true;
        st().context_switch(caller);
    }

    rsched_gunlock();
    with_internal_depth(|| (rpt().join)(thread, retval))
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
    let r = with_internal_depth(|| (rpt().mutex_lock)(lock));
    if r != 0 {
        st().clear_event(caller);
        rsched_gunlock();
        return r;
    }
    // Inform TSAN that this thread has logically acquired the user mutex.
    // Called while GMTX is still held so the annotation is ordered relative
    // to the paired tsan_release in rsched_pthread_mutex_unlock.
    tsan_user_acquire(lock as *mut libc::c_void);
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
            let r = with_internal_depth(|| (rpt().mutex_lock)(lock));
            if r != 0 {
                st().clear_event(caller);
                rsched_gunlock();
                return r;
            }
            tsan_user_acquire(lock as *mut libc::c_void);
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
    let r = with_internal_depth(|| (rpt().mutex_lock)(lock));
    if r != 0 {
        st().clear_event(caller);
        rsched_gunlock();
        return r;
    }
    tsan_user_acquire(lock as *mut libc::c_void);
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
    tsan_user_release(lock as *mut libc::c_void);
    let r = st().mutex_unlock(key, caller);
    if r == 0 {
        let real_r = with_internal_depth(|| (rpt().mutex_unlock)(lock));
        if real_r != 0 {
            st().clear_event(caller);
            rsched_gunlock();
            return real_r;
        }
    }
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
    let real_unlock = with_internal_depth(|| (rpt().mutex_unlock)(lock));
    if real_unlock != 0 {
        rsched_gunlock();
        return real_unlock;
    }

    st().conds
        .entry(ckey)
        .or_insert(SCond {
            waiters: Vec::new(),
        })
        .waiters
        .push(caller);
    st().t(caller).is_blocking = true;
    st().context_switch(caller);
    if st().t(caller).is_blocking {
        st().t(caller).is_in_rsched_wait = true;
        rsched_cond_wait(addr_of_mut!(st().t(caller).suspend_cond));
        st().t(caller).is_in_rsched_wait = false;
    }

    st().mutex_lock(lkey, caller);
    let real_lock = with_internal_depth(|| (rpt().mutex_lock)(lock));
    if real_lock != 0 {
        rsched_gunlock();
        return real_lock;
    }
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
        st().t(w).is_blocking = false;
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
            st().t(w).is_blocking = false;
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
    let r = with_internal_depth(|| (rpt().barrier_init)(barrier, attr, count));
    if r == 0 {
        st().barriers.insert(
            key,
            SBarrier {
                count,
                waiters: Vec::new(),
            },
        );
    }
    rsched_gunlock();
    r
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
            st().t(caller).is_blocking = true;
        }
    }

    // If we are the Nth thread, release all waiters.
    let mut released = false;
    {
        let b = st().barriers.get_mut(&key).unwrap();
        if b.waiters.len() >= b.count as usize {
            let ws: Vec<PthreadT> = b.waiters.drain(..).collect();
            for w in ws {
                st().t(w).is_blocking = false;
            }
            released = true;
        }
    }

    // If the barrier is not full yet, this caller must remain blocked until
    // the releasing thread marks all waiters runnable.  If this caller released
    // the barrier, yielding gives the waiters a chance to run.
    st().context_switch(caller);
    if st().t(caller).is_blocking {
        st().t(caller).is_in_rsched_wait = true;
        rsched_cond_wait(addr_of_mut!(st().t(caller).suspend_cond));
        st().t(caller).is_in_rsched_wait = false;
    }
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
    if is_in_rsched() {
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
