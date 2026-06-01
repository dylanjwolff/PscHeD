use crate::{AttrT, CondT, MutexT, PthreadT, StartArg};
use std::ffi::{CStr, CString};
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

const MAX_PROCESSES: usize = 32;
const MAX_TASKS_PER_PROCESS: usize = 64;
const MAX_TASKS: usize = MAX_PROCESSES * MAX_TASKS_PER_PROCESS;

#[derive(Clone, Copy)]
pub(crate) struct ParkingHandle(*mut CondT);

impl ParkingHandle {
    pub(crate) fn new(cond: *mut CondT) -> Self {
        Self(cond)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskChoice {
    pub(crate) task_id: usize,
    pub(crate) domain: i32,
    pub(crate) pthread: PthreadT,
}

pub(crate) struct TaskStatus {
    pub(crate) task_id: usize,
    pub(crate) pthread: PthreadT,
    pub(crate) is_blocking: bool,
    pub(crate) startup_done: bool,
    pub(crate) is_waiting: bool,
}

pub(crate) trait TaskProvider {
    unsafe fn create(
        &mut self,
        thread: *mut PthreadT,
        attr: *const AttrT,
        start_arg: *mut StartArg,
    ) -> libc::c_int;

    unsafe fn join(&mut self, thread: PthreadT, retval: *mut *mut libc::c_void) -> libc::c_int;

    unsafe fn wake(&mut self, handle: ParkingHandle) -> libc::c_int;

    unsafe fn park(&mut self, handle: ParkingHandle);

    unsafe fn global_lock(&mut self);

    unsafe fn global_unlock(&mut self);

    fn task_tid(&self, _thread: PthreadT) -> libc::pid_t {
        0
    }

    unsafe fn resume(&mut self, _thread: PthreadT) -> bool {
        false
    }

    unsafe fn switch_to(&mut self, _next: PthreadT, _caller: PthreadT) -> bool {
        false
    }

    fn starts_waiting(&self) -> bool {
        false
    }

    fn current_domain(&self) -> i32 {
        -1
    }

    unsafe fn register_current_domain(&mut self, _initially_runnable: bool) {}

    unsafe fn register_task(&mut self, _thread: PthreadT) -> usize {
        usize::MAX
    }

    unsafe fn update_task_status(
        &mut self,
        _task_id: usize,
        _is_blocking: bool,
        _startup_done: bool,
        _is_waiting: bool,
    ) {
    }

    unsafe fn publish_tasks(&mut self, _tasks: &[TaskStatus]) {}

    unsafe fn choose_domain_task(
        &mut self,
        _choose_index: &mut dyn FnMut(usize) -> Option<usize>,
    ) -> Option<TaskChoice> {
        None
    }

    unsafe fn switch_to_domain(&mut self, _choice: TaskChoice) -> bool {
        false
    }

    unsafe fn park_current_domain(&mut self) {}

    unsafe fn selected_domain_task(&mut self) -> Option<PthreadT> {
        None
    }

    unsafe fn fork(&mut self) -> libc::pid_t {
        crate::with_internal_depth(|| libc::fork())
    }

    unsafe fn before_fork(&mut self) {}

    unsafe fn after_fork_parent(&mut self, _child: libc::pid_t) -> bool {
        false
    }

    unsafe fn after_fork_child(&mut self) {}

    unsafe fn process_exit(&mut self, _next: Option<TaskChoice>) {}

    unsafe fn execv(
        &mut self,
        path: *const libc::c_char,
        argv: *const *const libc::c_char,
    ) -> libc::c_int {
        crate::with_internal_depth(|| libc::execv(path, argv))
    }

    unsafe fn execve(
        &mut self,
        path: *const libc::c_char,
        argv: *const *const libc::c_char,
        envp: *const *const libc::c_char,
    ) -> libc::c_int {
        crate::with_internal_depth(|| libc::execve(path, argv, envp))
    }
}

pub(crate) struct ThreadTaskProvider;

impl ThreadTaskProvider {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TaskProvider for ThreadTaskProvider {
    unsafe fn create(
        &mut self,
        thread: *mut PthreadT,
        attr: *const AttrT,
        start_arg: *mut StartArg,
    ) -> libc::c_int {
        #[cfg(feature = "asan")]
        {
            crate::with_internal_depth(|| {
                libc::pthread_create(thread, attr, crate::trampoline, start_arg.cast())
            })
        }
        #[cfg(not(feature = "asan"))]
        {
            crate::with_internal_depth(|| {
                (crate::rpt().create)(thread, attr, crate::trampoline, start_arg.cast())
            })
        }
    }

    unsafe fn join(&mut self, thread: PthreadT, retval: *mut *mut libc::c_void) -> libc::c_int {
        crate::with_internal_depth(|| (crate::rpt().join)(thread, retval))
    }

    unsafe fn wake(&mut self, handle: ParkingHandle) -> libc::c_int {
        crate::with_internal_depth(|| (crate::rpt().cond_signal)(handle.0))
    }

    unsafe fn park(&mut self, handle: ParkingHandle) {
        crate::thread_cond_wait(handle.0);
    }

    unsafe fn global_lock(&mut self) {
        crate::thread_glock();
    }

    unsafe fn global_unlock(&mut self) {
        crate::thread_gunlock();
    }
}

mod coro {
    use super::{ParkingHandle, TaskProvider};
    use crate::{AttrT, PthreadT, StartArg};
    use corosensei::{Coroutine, CoroutineResult, stack::DefaultStack};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicI32, Ordering};

    enum CoroYield {
        Yielded,
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const ARCH_SET_FS: libc::c_int = 0x1002;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const ARCH_GET_FS: libc::c_int = 0x1003;

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe fn get_fs_base() -> usize {
        let mut base = 0usize;
        let r = libc::syscall(libc::SYS_arch_prctl, ARCH_GET_FS, &mut base);
        assert_eq!(r, 0, "rsched: ARCH_GET_FS failed");
        base
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    unsafe fn get_fs_base() -> usize {
        0
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe fn set_fs_base(base: usize) {
        let r = libc::syscall(libc::SYS_arch_prctl, ARCH_SET_FS, base);
        assert_eq!(r, 0, "rsched: ARCH_SET_FS failed");
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    unsafe fn set_fs_base(_base: usize) {}

    #[cfg(target_os = "linux")]
    unsafe fn futex_wait(addr: *const AtomicI32, expected: i32) {
        const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
        loop {
            let r = libc::syscall(
                libc::SYS_futex,
                addr as *const i32,
                FUTEX_WAIT_PRIVATE,
                expected,
                core::ptr::null::<libc::timespec>(),
            );
            if r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN) {
                return;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return;
            }
        }
    }

    #[cfg(target_os = "linux")]
    unsafe fn futex_wake(addr: *const AtomicI32) {
        const FUTEX_WAKE_PRIVATE: libc::c_int = 129;
        libc::syscall(libc::SYS_futex, addr as *const i32, FUTEX_WAKE_PRIVATE, 1);
    }

    unsafe extern "C" {
        fn pthread_attr_getdetachstate(attr: *const AttrT, state: *mut libc::c_int) -> libc::c_int;
    }

    thread_local! {
        static CORO_CONTEXT: Cell<*const CoroContext> =
            const { Cell::new(core::ptr::null()) };
    }

    struct CoroContext {
        yielder: Cell<*const corosensei::Yielder<(), CoroYield>>,
        fs_base: Cell<usize>,
        host_fs_base: Cell<usize>,
    }

    impl CoroContext {
        fn new(fs_base: usize) -> Self {
            Self {
                yielder: Cell::new(core::ptr::null()),
                fs_base: Cell::new(fs_base),
                host_fs_base: Cell::new(0),
            }
        }
    }

    struct ShadowThread {
        state: AtomicI32,
        pthread: PthreadT,
        tid: libc::pid_t,
        fs_base: usize,
    }

    impl ShadowThread {
        unsafe fn new() -> *mut Self {
            Box::into_raw(Box::new(Self {
                state: AtomicI32::new(0),
                pthread: core::mem::zeroed(),
                tid: 0,
                fs_base: 0,
            }))
        }

        unsafe fn shutdown(shadow: *mut Self) {
            if shadow.is_null() {
                return;
            }

            (*shadow).state.store(2, Ordering::Release);
            futex_wake(std::ptr::addr_of!((*shadow).state));

            crate::with_internal_depth(|| {
                (crate::rpt().join)((*shadow).pthread, core::ptr::null_mut())
            });

            drop(Box::from_raw(shadow));
        }
    }

    extern "C" fn shadow_thread_main(raw: *mut libc::c_void) -> *mut libc::c_void {
        unsafe {
            let shadow = raw as *mut ShadowThread;

            (*shadow).pthread = libc::pthread_self();
            (*shadow).tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
            (*shadow).fs_base = get_fs_base();
            (*shadow).state.store(1, Ordering::Release);
            futex_wake(std::ptr::addr_of!((*shadow).state));

            while (*shadow).state.load(Ordering::Acquire) != 2 {
                futex_wait(std::ptr::addr_of!((*shadow).state), 1);
            }

            core::ptr::null_mut()
        }
    }

    struct CoroTask {
        coroutine: Coroutine<(), CoroYield, *mut libc::c_void>,
        context: Rc<CoroContext>,
        shadow: *mut ShadowThread,
        retval: Option<*mut libc::c_void>,
        tid: libc::pid_t,
    }

    pub(crate) struct CoroTaskProvider {
        tasks: HashMap<PthreadT, CoroTask>,
        requested_next: Option<PthreadT>,
    }

    impl CoroTaskProvider {
        pub(crate) fn new() -> Self {
            Self {
                tasks: HashMap::new(),
                requested_next: None,
            }
        }

        unsafe fn create_shadow(
            &mut self,
            attr: *const AttrT,
        ) -> Result<(*mut ShadowThread, PthreadT, libc::pid_t, usize), libc::c_int> {
            let mut detached = false;
            if !attr.is_null() {
                let mut detach_state = 0;
                let r = crate::with_internal_depth(|| {
                    pthread_attr_getdetachstate(attr, &mut detach_state)
                });
                if r == 0 {
                    detached = detach_state == libc::PTHREAD_CREATE_DETACHED;
                }
            }

            let shadow = ShadowThread::new();
            let mut shadow_pt: PthreadT = core::mem::zeroed();
            let mut attr_copy = core::mem::MaybeUninit::<AttrT>::uninit();
            let create_attr = if detached && !attr.is_null() {
                attr_copy.write(core::ptr::read(attr));
                let attr_copy_ptr = attr_copy.as_mut_ptr();
                crate::with_internal_depth(|| {
                    libc::pthread_attr_setdetachstate(attr_copy_ptr, libc::PTHREAD_CREATE_JOINABLE)
                });
                attr_copy_ptr as *const AttrT
            } else {
                attr
            };
            #[cfg(feature = "asan")]
            let r = crate::with_internal_depth(|| {
                libc::pthread_create(
                    &mut shadow_pt,
                    create_attr,
                    shadow_thread_main,
                    shadow.cast(),
                )
            });
            #[cfg(not(feature = "asan"))]
            let r = crate::with_internal_depth(|| {
                (crate::rpt().create)(
                    &mut shadow_pt,
                    create_attr,
                    shadow_thread_main,
                    shadow.cast(),
                )
            });
            if r != 0 {
                drop(Box::from_raw(shadow));
                return Err(r);
            }

            while (*shadow).state.load(Ordering::Acquire) == 0 {
                futex_wait(std::ptr::addr_of!((*shadow).state), 0);
            }
            let pt = (*shadow).pthread;
            let tid = (*shadow).tid;
            let fs_base = (*shadow).fs_base;

            Ok((shadow, pt, tid, fs_base))
        }

        unsafe fn resume_task(&mut self, pt: PthreadT) {
            let Some(mut task) = self.tasks.remove(&pt) else {
                return;
            };
            if task.retval.is_some() {
                self.tasks.insert(pt, task);
                return;
            }
            let previous_pt = crate::MY_PT.with(|c| {
                let previous = *c.borrow();
                *c.borrow_mut() = pt;
                previous
            });
            let host_fs_base = get_fs_base();
            task.context.host_fs_base.set(host_fs_base);
            set_fs_base(task.context.fs_base.get());
            match task.coroutine.resume(()) {
                CoroutineResult::Yield(CoroYield::Yielded) => {}
                CoroutineResult::Return(retval) => {
                    task.retval = Some(retval);
                }
            }
            set_fs_base(host_fs_base);
            crate::MY_PT.with(|c| *c.borrow_mut() = previous_pt);
            self.tasks.insert(pt, task);
        }

        pub(crate) unsafe fn suspend_current() {
            let ctx = CORO_CONTEXT.with(|cell| cell.get());
            if ctx.is_null() {
                return;
            }
            let yielder = (*ctx).yielder.get();
            if yielder.is_null() {
                return;
            }

            (*ctx).fs_base.set(get_fs_base());
            set_fs_base((*ctx).host_fs_base.get());
            (&*yielder).suspend(CoroYield::Yielded);
        }

        unsafe fn finish_task(&mut self, mut task: CoroTask) {
            ShadowThread::shutdown(task.shadow);
            task.shadow = core::ptr::null_mut();
            core::mem::forget(task);
        }
    }

    impl Drop for CoroTaskProvider {
        fn drop(&mut self) {
            for (_, mut task) in self.tasks.drain() {
                unsafe {
                    ShadowThread::shutdown(task.shadow);
                    task.shadow = core::ptr::null_mut();
                    core::mem::forget(task);
                }
            }
        }
    }

    impl TaskProvider for CoroTaskProvider {
        unsafe fn create(
            &mut self,
            thread: *mut PthreadT,
            attr: *const AttrT,
            start_arg: *mut StartArg,
        ) -> libc::c_int {
            let (shadow, pt, tid, fs_base) = match self.create_shadow(attr) {
                Ok(shadow) => shadow,
                Err(r) => return r,
            };
            *thread = pt;
            let routine = (*start_arg).routine;
            let arg = (*start_arg).arg;
            (*start_arg).ready = true;
            let context = Rc::new(CoroContext::new(fs_base));
            let context_for_coro = context.clone();

            let coroutine = Coroutine::with_stack(
                DefaultStack::new(2 * 1024 * 1024).unwrap(),
                move |yielder, ()| {
                    context_for_coro.yielder.set(yielder as *const _);
                    CORO_CONTEXT.with(|cell| cell.set(Rc::as_ptr(&context_for_coro)));
                    crate::MY_PT.with(|c| *c.borrow_mut() = pt);
                    let retval = routine(arg);
                    crate::do_thread_exit(pt);
                    context_for_coro.fs_base.set(get_fs_base());
                    CORO_CONTEXT.with(|cell| cell.set(core::ptr::null()));
                    set_fs_base(context_for_coro.host_fs_base.get());
                    retval
                },
            );
            self.tasks.insert(
                pt,
                CoroTask {
                    coroutine,
                    context,
                    shadow,
                    retval: None,
                    tid,
                },
            );
            0
        }

        unsafe fn join(&mut self, thread: PthreadT, retval: *mut *mut libc::c_void) -> libc::c_int {
            let Some(task) = self.tasks.remove(&thread) else {
                return 0;
            };
            if !retval.is_null() {
                *retval = task.retval.unwrap_or(core::ptr::null_mut());
            }
            self.finish_task(task);
            0
        }

        unsafe fn wake(&mut self, _handle: ParkingHandle) -> libc::c_int {
            0
        }

        unsafe fn park(&mut self, _handle: ParkingHandle) {
            Self::suspend_current();
        }

        unsafe fn global_lock(&mut self) {}

        unsafe fn global_unlock(&mut self) {}

        unsafe fn resume(&mut self, thread: PthreadT) -> bool {
            if !self.tasks.contains_key(&thread) {
                return false;
            }
            self.resume_task(thread);
            true
        }

        unsafe fn switch_to(&mut self, next: PthreadT, caller: PthreadT) -> bool {
            if !self.tasks.contains_key(&next) {
                return false;
            }

            let in_coro = CORO_CONTEXT.with(|cell| !cell.get().is_null());
            if in_coro {
                self.requested_next = Some(next);
                Self::suspend_current();
                return true;
            }

            let mut selected = next;
            loop {
                self.resume_task(selected);
                match self.requested_next.take() {
                    Some(pt) if pt != caller && self.tasks.contains_key(&pt) => {
                        selected = pt;
                    }
                    _ => break,
                }
            }
            true
        }

        fn starts_waiting(&self) -> bool {
            true
        }

        fn task_tid(&self, thread: PthreadT) -> libc::pid_t {
            self.tasks.get(&thread).map_or(0, |task| task.tid)
        }
    }
}

pub(crate) use coro::CoroTaskProvider;

static PROCESS_SHARED: AtomicPtr<ProcessShared> = AtomicPtr::new(core::ptr::null_mut());
static PROCESS_SLOT: AtomicI32 = AtomicI32::new(-1);
static FORK_SLOT: AtomicI32 = AtomicI32::new(-1);
static PROCESS_SHARED_FD: AtomicI32 = AtomicI32::new(-1);

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
    domain: i32,
    pthread: usize,
}

#[repr(C)]
struct ProcessShared {
    lock: MutexT,
    task_count: usize,
    tasks: [SharedTask; MAX_TASKS],
    slots: [ProcessSlot; MAX_PROCESSES],
}

unsafe fn process_shared_ptr() -> *mut ProcessShared {
    let mut p = PROCESS_SHARED.load(Ordering::Acquire);
    if !p.is_null() {
        return p;
    }

    let size = core::mem::size_of::<ProcessShared>();
    if let Ok(raw_fd) = std::env::var("RSCHED_SHM_FD")
        && let Ok(fd) = raw_fd.parse::<libc::c_int>()
    {
        let raw = libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        assert_ne!(
            raw,
            libc::MAP_FAILED,
            "rsched: mmap inherited shared process state failed"
        );
        p = raw.cast::<ProcessShared>();
        PROCESS_SHARED_FD.store(fd, Ordering::Release);
        PROCESS_SHARED.store(p, Ordering::Release);
        return p;
    }

    let fd =
        libc::syscall(libc::SYS_memfd_create, c"rsched-process-shared".as_ptr(), 0) as libc::c_int;
    assert!(fd >= 0, "rsched: memfd_create failed");
    let r = libc::ftruncate(fd, size as libc::off_t);
    assert_eq!(r, 0, "rsched: ftruncate shared process state failed");

    let raw = libc::mmap(
        core::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    assert_ne!(
        raw,
        libc::MAP_FAILED,
        "rsched: mmap shared process state failed"
    );
    core::ptr::write_bytes(raw, 0, size);
    p = raw.cast::<ProcessShared>();
    PROCESS_SHARED_FD.store(fd, Ordering::Release);
    set_env_usize("RSCHED_SHM_FD", fd as usize);

    let mut attr: libc::pthread_mutexattr_t = core::mem::zeroed();
    let r = crate::with_internal_depth(|| libc::pthread_mutexattr_init(&mut attr));
    assert_eq!(r, 0, "rsched: pthread_mutexattr_init failed");
    let r = crate::with_internal_depth(|| {
        libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED)
    });
    assert_eq!(r, 0, "rsched: pthread_mutexattr_setpshared failed");
    let r = crate::with_internal_depth(|| libc::pthread_mutex_init(addr_of_mut!((*p).lock), &attr));
    assert_eq!(r, 0, "rsched: process-shared mutex init failed");
    crate::with_internal_depth(|| libc::pthread_mutexattr_destroy(&mut attr));

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

fn set_env_usize(key: &str, value: usize) {
    let key = CString::new(key).expect("rsched env key contains nul");
    let value = CString::new(value.to_string()).expect("rsched env value contains nul");
    unsafe {
        let r = libc::setenv(key.as_ptr(), value.as_ptr(), 1);
        assert_eq!(r, 0, "rsched: setenv failed");
    }
}

unsafe fn envp_with_rsched_vars(
    envp: *const *const libc::c_char,
) -> (Vec<CString>, Vec<*const libc::c_char>) {
    let mut owned = Vec::new();
    let mut ptrs = Vec::new();
    let rsched_prefixes: [&[u8]; 3] = [
        b"RSCHED_SHM_FD=",
        b"RSCHED_PROCESS_SLOT=",
        b"RSCHED_FORK_SLOT=",
    ];

    if !envp.is_null() {
        let mut i = 0;
        loop {
            let p = *envp.add(i);
            if p.is_null() {
                break;
            }
            let bytes = CStr::from_ptr(p).to_bytes();
            if !rsched_prefixes
                .iter()
                .any(|prefix| bytes.starts_with(prefix))
            {
                ptrs.push(p);
            }
            i += 1;
        }
    }

    for key in ["RSCHED_SHM_FD", "RSCHED_PROCESS_SLOT"] {
        if let Ok(value) = std::env::var(key) {
            owned.push(
                CString::new(format!("{key}={value}")).expect("rsched env value contains nul"),
            );
        }
    }
    ptrs.extend(owned.iter().map(|s| s.as_ptr()));
    ptrs.push(core::ptr::null());
    (owned, ptrs)
}

fn process_log(args: core::fmt::Arguments<'_>) {
    if std::env::var("RSCHED_PROCESS_LOG").is_ok_and(|v| v == "1") {
        eprintln!("[rsched-process] {args}");
    }
}

unsafe fn process_lock(ps: *mut ProcessShared) {
    let r = crate::with_internal_depth(|| (crate::rpt().mutex_lock)(addr_of_mut!((*ps).lock)));
    assert_eq!(r, 0, "rsched: process shared lock failed");
}

unsafe fn process_unlock(ps: *mut ProcessShared) {
    let r = crate::with_internal_depth(|| (crate::rpt().mutex_unlock)(addr_of_mut!((*ps).lock)));
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

unsafe fn deactivate_process_tasks_locked(ps: *mut ProcessShared, domain: i32) {
    for id in 0..(*ps).task_count.min(MAX_TASKS) {
        if (*ps).tasks[id].domain == domain {
            (*ps).tasks[id].active = 0;
            (*ps).tasks[id].is_blocking = 1;
            (*ps).tasks[id].is_waiting = 0;
        }
    }
}

unsafe fn process_wait_on_slot(ps: *mut ProcessShared, slot: i32) {
    loop {
        let r = crate::with_internal_depth(|| {
            libc::sem_wait(addr_of_mut!((*ps).slots[slot as usize].gate))
        });
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
                if task.active != 0 && task.domain == slot {
                    ready = true;
                    break;
                }
            }
        }
        process_unlock(ps);
        if ready {
            return;
        }
        crate::with_internal_depth(|| libc::sched_yield());
    }
}

pub(crate) struct ProcessTaskProvider {
    inner: Box<dyn TaskProvider>,
}

impl ProcessTaskProvider {
    pub(crate) fn new(inner: Box<dyn TaskProvider>) -> Self {
        Self { inner }
    }

    unsafe fn update_process_task_status(
        &mut self,
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

    unsafe fn register_process_task(&mut self, pt: PthreadT) -> usize {
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
            domain: slot,
            pthread: pt as usize,
        };
        process_unlock(ps);
        task_id
    }

    unsafe fn publish_process_tasks(&mut self, tasks: &[TaskStatus]) {
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() {
            return;
        }
        process_lock(ps);
        for task in tasks {
            set_task_status_locked(
                ps,
                task.task_id,
                task.is_blocking,
                task.startup_done,
                task.is_waiting,
            );
            if task.task_id < (*ps).task_count && task.task_id < MAX_TASKS {
                (*ps).tasks[task.task_id].pthread = task.pthread as usize;
            }
        }
        process_unlock(ps);
    }

    unsafe fn choose_domain_task(
        &mut self,
        choose_index: &mut dyn FnMut(usize) -> Option<usize>,
    ) -> Option<TaskChoice> {
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() {
            return None;
        }
        let mut tasks = [TaskChoice {
            task_id: usize::MAX,
            domain: -1,
            pthread: 0 as PthreadT,
        }; MAX_TASKS];
        let mut n = 0usize;
        process_lock(ps);
        for id in 0..(*ps).task_count.min(MAX_TASKS) {
            let task = (*ps).tasks[id];
            if task.active != 0
                && task.domain >= 0
                && (*ps).slots[task.domain as usize].active != 0
                && task.is_blocking == 0
                && task.startup_done != 0
                && task.is_waiting != 0
            {
                tasks[n] = TaskChoice {
                    task_id: id,
                    domain: task.domain,
                    pthread: task.pthread as PthreadT,
                };
                n += 1;
            }
        }
        let choice = choose_index(n).map(|idx| tasks[idx]);
        process_unlock(ps);
        choice
    }

    unsafe fn switch_to_domain(&mut self, choice: TaskChoice) -> bool {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() || current < 0 {
            return false;
        }

        process_lock(ps);
        if choice.domain < 0 || (*ps).slots[choice.domain as usize].active == 0 {
            process_unlock(ps);
            return false;
        }
        (*ps).slots[choice.domain as usize].selected_task = choice.task_id;
        process_log(format_args!(
            "pid {} switch slot {} -> {} thread {:#x}",
            libc::getpid(),
            current,
            choice.domain,
            choice.pthread as usize
        ));
        let r = crate::with_internal_depth(|| {
            libc::sem_post(addr_of_mut!((*ps).slots[choice.domain as usize].gate))
        });
        assert_eq!(r, 0, "rsched: sem_post failed");
        process_unlock(ps);

        true
    }

    unsafe fn park_current_domain(&mut self) {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if current >= 0 && !ps.is_null() {
            process_wait_on_slot(ps, current);
        }
    }

    unsafe fn selected_domain_task(&mut self) -> Option<PthreadT> {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if current < 0 || ps.is_null() {
            return None;
        }
        process_lock(ps);
        let selected_task = (*ps).slots[current as usize].selected_task;
        (*ps).slots[current as usize].selected_task = usize::MAX;
        let selected = if selected_task < (*ps).task_count {
            Some((*ps).tasks[selected_task].pthread as PthreadT)
        } else {
            None
        };
        process_unlock(ps);
        process_log(format_args!(
            "pid {} woke slot {} selected task {} thread {:#x}",
            libc::getpid(),
            current,
            selected_task,
            selected.unwrap_or(0 as PthreadT) as usize
        ));
        selected
    }

    unsafe fn register_current(&mut self, initially_runnable: bool) -> i32 {
        let ps = process_shared_ptr();
        process_lock(ps);
        let pid = libc::getpid();

        if let Ok(raw_slot) = std::env::var("RSCHED_PROCESS_SLOT")
            && let Ok(slot) = raw_slot.parse::<usize>()
            && slot < MAX_PROCESSES
            && (*ps).slots[slot].active != 0
        {
            (*ps).slots[slot].pid = pid;
            process_unlock(ps);
            PROCESS_SLOT.store(slot as i32, Ordering::Release);
            return slot as i32;
        }

        for i in 0..MAX_PROCESSES {
            if (*ps).slots[i].active != 0 && (*ps).slots[i].pid == pid {
                process_unlock(ps);
                PROCESS_SLOT.store(i as i32, Ordering::Release);
                set_env_usize("RSCHED_PROCESS_SLOT", i);
                return i as i32;
            }
        }

        for i in 0..MAX_PROCESSES {
            if (*ps).slots[i].active == 0 {
                let r = crate::with_internal_depth(|| {
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
                set_env_usize("RSCHED_PROCESS_SLOT", i);
                if initially_runnable {
                    let mut discard = 0;
                    while crate::with_internal_depth(|| {
                        libc::sem_trywait(addr_of_mut!((*ps).slots[i].gate))
                    }) == 0
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

    pub(crate) unsafe fn deactivate_current_domain_tasks() {
        let current_slot = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if current_slot >= 0 && !ps.is_null() {
            process_lock(ps);
            deactivate_process_tasks_locked(ps, current_slot);
            process_unlock(ps);
        }
    }

    pub(crate) unsafe fn exit_current_domain(next: Option<TaskChoice>) {
        Self::process_exit_impl(next);
    }

    unsafe fn before_fork_impl(&mut self) {
        let ps = process_shared_ptr();
        process_lock(ps);
        for i in 0..MAX_PROCESSES {
            if (*ps).slots[i].active == 0 {
                let r = crate::with_internal_depth(|| {
                    libc::sem_init(addr_of_mut!((*ps).slots[i].gate), 1, 0)
                });
                assert_eq!(r, 0, "rsched: fork sem_init failed");
                (*ps).slots[i].pid = 0;
                (*ps).slots[i].active = 1;
                (*ps).slots[i].selected_task = usize::MAX;
                FORK_SLOT.store(i as i32, Ordering::Release);
                set_env_usize("RSCHED_FORK_SLOT", i);
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

    unsafe fn after_fork_parent_impl(&mut self, child: libc::pid_t) -> bool {
        let slot = FORK_SLOT.swap(-1, Ordering::AcqRel);
        if slot < 0 {
            return false;
        }
        let ps = process_shared_ptr();
        process_lock(ps);
        if child <= 0 {
            (*ps).slots[slot as usize].active = 0;
            process_unlock(ps);
            return false;
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
        true
    }

    unsafe fn after_fork_child_impl(&mut self) {
        let slot = FORK_SLOT.swap(-1, Ordering::AcqRel);
        let ps = process_shared_ptr();
        if slot >= 0 {
            process_lock(ps);
            (*ps).slots[slot as usize].pid = libc::getpid();
            set_env_usize("RSCHED_PROCESS_SLOT", slot as usize);
            process_log(format_args!(
                "pid {} child using fork slot {}",
                libc::getpid(),
                slot
            ));
            process_unlock(ps);
            PROCESS_SLOT.store(slot, Ordering::Release);
        } else {
            self.register_current(false);
        }
        crate::reset_local_state_after_fork();
        crate::rsched_glock();
        crate::st().publish_local_tasks(crate::my_pt());
        crate::rsched_gunlock();
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

    unsafe fn process_exit_impl(next: Option<TaskChoice>) {
        let current = PROCESS_SLOT.swap(-1, Ordering::AcqRel);
        if current < 0 {
            return;
        }
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if ps.is_null() {
            return;
        }
        process_lock(ps);
        (*ps).slots[current as usize].active = 0;
        deactivate_process_tasks_locked(ps, current);
        process_unlock(ps);
        process_log(format_args!(
            "pid {} exit slot {}, next slot {} thread {:#x}",
            libc::getpid(),
            current,
            next.map_or(-1, |choice| choice.domain),
            next.map_or(0, |choice| choice.pthread as usize)
        ));
        if let Some(choice) = next
            && choice.domain != current
            && choice.domain >= 0
        {
            process_lock(ps);
            (*ps).slots[choice.domain as usize].selected_task = choice.task_id;
            let _ = crate::with_internal_depth(|| {
                libc::sem_post(addr_of_mut!((*ps).slots[choice.domain as usize].gate))
            });
            process_unlock(ps);
        }
    }

    unsafe fn prepare_exec(&mut self) {
        let current = PROCESS_SLOT.load(Ordering::Acquire);
        let ps = PROCESS_SHARED.load(Ordering::Acquire);
        if current >= 0 && !ps.is_null() {
            process_log(format_args!(
                "pid {} preparing exec in slot {}",
                libc::getpid(),
                current
            ));
            process_lock(ps);
            deactivate_process_tasks_locked(ps, current);
            (*ps).slots[current as usize].selected_task = usize::MAX;
            process_unlock(ps);
            set_env_usize("RSCHED_PROCESS_SLOT", current as usize);
        }
    }
}

impl TaskProvider for ProcessTaskProvider {
    unsafe fn create(
        &mut self,
        thread: *mut PthreadT,
        attr: *const AttrT,
        start_arg: *mut StartArg,
    ) -> libc::c_int {
        self.inner.create(thread, attr, start_arg)
    }

    unsafe fn join(&mut self, thread: PthreadT, retval: *mut *mut libc::c_void) -> libc::c_int {
        self.inner.join(thread, retval)
    }

    unsafe fn wake(&mut self, handle: ParkingHandle) -> libc::c_int {
        self.inner.wake(handle)
    }

    unsafe fn park(&mut self, handle: ParkingHandle) {
        self.inner.park(handle);
    }

    unsafe fn global_lock(&mut self) {
        self.inner.global_lock();
    }

    unsafe fn global_unlock(&mut self) {
        self.inner.global_unlock();
    }

    fn task_tid(&self, thread: PthreadT) -> libc::pid_t {
        self.inner.task_tid(thread)
    }

    unsafe fn resume(&mut self, thread: PthreadT) -> bool {
        self.inner.resume(thread)
    }

    unsafe fn switch_to(&mut self, next: PthreadT, caller: PthreadT) -> bool {
        self.inner.switch_to(next, caller)
    }

    fn starts_waiting(&self) -> bool {
        self.inner.starts_waiting()
    }

    fn current_domain(&self) -> i32 {
        PROCESS_SLOT.load(Ordering::Acquire)
    }

    unsafe fn register_current_domain(&mut self, initially_runnable: bool) {
        self.register_current(initially_runnable);
    }

    unsafe fn register_task(&mut self, thread: PthreadT) -> usize {
        self.register_process_task(thread)
    }

    unsafe fn update_task_status(
        &mut self,
        task_id: usize,
        is_blocking: bool,
        startup_done: bool,
        is_waiting: bool,
    ) {
        self.update_process_task_status(task_id, is_blocking, startup_done, is_waiting);
    }

    unsafe fn publish_tasks(&mut self, tasks: &[TaskStatus]) {
        self.publish_process_tasks(tasks);
    }

    unsafe fn choose_domain_task(
        &mut self,
        choose_index: &mut dyn FnMut(usize) -> Option<usize>,
    ) -> Option<TaskChoice> {
        self.choose_domain_task(choose_index)
    }

    unsafe fn switch_to_domain(&mut self, choice: TaskChoice) -> bool {
        self.switch_to_domain(choice)
    }

    unsafe fn park_current_domain(&mut self) {
        self.park_current_domain();
    }

    unsafe fn selected_domain_task(&mut self) -> Option<PthreadT> {
        self.selected_domain_task()
    }

    unsafe fn fork(&mut self) -> libc::pid_t {
        self.before_fork();
        let pid = crate::with_internal_depth(|| libc::fork());
        if pid == 0 {
            self.after_fork_child();
        } else if self.after_fork_parent(pid) {
            crate::rsched_glock();
            let caller = crate::my_pt();
            crate::st().context_switch(caller);
            crate::rsched_gunlock();
        }
        pid
    }

    unsafe fn before_fork(&mut self) {
        self.before_fork_impl();
    }

    unsafe fn after_fork_parent(&mut self, child: libc::pid_t) -> bool {
        self.after_fork_parent_impl(child)
    }

    unsafe fn after_fork_child(&mut self) {
        self.after_fork_child_impl();
    }

    unsafe fn process_exit(&mut self, next: Option<TaskChoice>) {
        Self::process_exit_impl(next);
    }

    unsafe fn execv(
        &mut self,
        path: *const libc::c_char,
        argv: *const *const libc::c_char,
    ) -> libc::c_int {
        self.prepare_exec();
        crate::with_internal_depth(|| libc::execv(path, argv))
    }

    unsafe fn execve(
        &mut self,
        path: *const libc::c_char,
        argv: *const *const libc::c_char,
        envp: *const *const libc::c_char,
    ) -> libc::c_int {
        self.prepare_exec();
        let (_owned_env, merged_envp) = envp_with_rsched_vars(envp);
        crate::with_internal_depth(|| libc::execve(path, argv, merged_envp.as_ptr()))
    }
}

pub(crate) unsafe fn deactivate_current_domain_tasks() {
    ProcessTaskProvider::deactivate_current_domain_tasks();
}

pub(crate) unsafe fn exit_current_domain(next: Option<TaskChoice>) {
    ProcessTaskProvider::exit_current_domain(next);
}

pub(crate) fn default_task_provider() -> Box<dyn TaskProvider> {
    let local: Box<dyn TaskProvider> = if cfg!(feature = "coro") {
        Box::new(CoroTaskProvider::new())
    } else {
        Box::new(ThreadTaskProvider::new())
    };
    Box::new(ProcessTaskProvider::new(local))
}
