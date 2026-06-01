use crate::{AttrT, CondT, PthreadT, StartArg};

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
    pub(crate) process_slot: i32,
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

    fn current_process_slot(&self) -> i32 {
        -1
    }

    unsafe fn register_current_process(&mut self, _initially_runnable: bool) {}

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

    unsafe fn choose_process_task(
        &mut self,
        _choose_index: &mut dyn FnMut(usize) -> Option<usize>,
    ) -> Option<TaskChoice> {
        None
    }

    unsafe fn switch_to_process(&mut self, _choice: TaskChoice) -> bool {
        false
    }

    unsafe fn park_current_process(&mut self) {}

    unsafe fn selected_process_task(&mut self) -> Option<PthreadT> {
        None
    }

    unsafe fn fork(&mut self) -> libc::pid_t {
        crate::with_internal_depth(|| libc::fork())
    }

    unsafe fn after_fork_child(&mut self) {}

    unsafe fn prepare_exec(&mut self) {}
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

    enum CoroYield {
        Yielded,
    }

    thread_local! {
        static CORO_YIELDER: Cell<*const corosensei::Yielder<(), CoroYield>> =
            const { Cell::new(core::ptr::null()) };
    }

    struct CoroTask {
        coroutine: Coroutine<(), CoroYield, *mut libc::c_void>,
        yielder: Rc<Cell<*const corosensei::Yielder<(), CoroYield>>>,
        retval: Option<*mut libc::c_void>,
        tid: libc::pid_t,
    }

    pub(crate) struct CoroTaskProvider {
        next_pthread: usize,
        next_tid: libc::pid_t,
        tasks: HashMap<PthreadT, CoroTask>,
        requested_next: Option<PthreadT>,
    }

    impl CoroTaskProvider {
        pub(crate) fn new() -> Self {
            Self {
                next_pthread: 1usize << (usize::BITS - 2),
                next_tid: 1,
                tasks: HashMap::new(),
                requested_next: None,
            }
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
            CORO_YIELDER.with(|cell| cell.set(task.yielder.get()));
            match task.coroutine.resume(()) {
                CoroutineResult::Yield(CoroYield::Yielded) => {}
                CoroutineResult::Return(retval) => {
                    task.retval = Some(retval);
                }
            }
            CORO_YIELDER.with(|cell| cell.set(core::ptr::null()));
            crate::MY_PT.with(|c| *c.borrow_mut() = previous_pt);
            self.tasks.insert(pt, task);
        }

        pub(crate) unsafe fn suspend_current() {
            CORO_YIELDER.with(|cell| {
                let yielder = cell.get();
                if !yielder.is_null() {
                    unsafe {
                        (&*yielder).suspend(CoroYield::Yielded);
                    }
                }
            });
        }
    }

    impl Drop for CoroTaskProvider {
        fn drop(&mut self) {
            for (_, task) in self.tasks.drain() {
                core::mem::forget(task);
            }
        }
    }

    impl TaskProvider for CoroTaskProvider {
        unsafe fn create(
            &mut self,
            thread: *mut PthreadT,
            _attr: *const AttrT,
            start_arg: *mut StartArg,
        ) -> libc::c_int {
            let pt = self.next_pthread as PthreadT;
            self.next_pthread += 1;
            let tid = self.next_tid;
            self.next_tid += 1;
            *thread = pt;
            let routine = (*start_arg).routine;
            let arg = (*start_arg).arg;
            (*start_arg).ready = true;
            let yielder_cell = Rc::new(Cell::new(core::ptr::null()));
            let yielder_for_coro = yielder_cell.clone();

            let coroutine = Coroutine::with_stack(
                DefaultStack::new(2 * 1024 * 1024).unwrap(),
                move |yielder, ()| {
                    yielder_for_coro.set(yielder as *const _);
                    CORO_YIELDER.with(|cell| cell.set(yielder as *const _));
                    crate::MY_PT.with(|c| *c.borrow_mut() = pt);
                    let retval = routine(arg);
                    crate::do_thread_exit(pt);
                    CORO_YIELDER.with(|cell| cell.set(core::ptr::null()));
                    retval
                },
            );
            self.tasks.insert(
                pt,
                CoroTask {
                    coroutine,
                    yielder: yielder_cell,
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
            core::mem::forget(task);
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

            let in_coro = CORO_YIELDER.with(|cell| !cell.get().is_null());
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

pub(crate) struct ProcessTaskProvider {
    inner: Box<dyn TaskProvider>,
}

impl ProcessTaskProvider {
    pub(crate) fn new(inner: Box<dyn TaskProvider>) -> Self {
        Self { inner }
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

    fn current_process_slot(&self) -> i32 {
        crate::process_current_slot()
    }

    unsafe fn register_current_process(&mut self, initially_runnable: bool) {
        crate::process_register_current(initially_runnable);
    }

    unsafe fn register_task(&mut self, thread: PthreadT) -> usize {
        crate::register_process_task(thread)
    }

    unsafe fn update_task_status(
        &mut self,
        task_id: usize,
        is_blocking: bool,
        startup_done: bool,
        is_waiting: bool,
    ) {
        crate::update_process_task_status(task_id, is_blocking, startup_done, is_waiting);
    }

    unsafe fn publish_tasks(&mut self, tasks: &[TaskStatus]) {
        crate::publish_process_tasks(tasks);
    }

    unsafe fn choose_process_task(
        &mut self,
        choose_index: &mut dyn FnMut(usize) -> Option<usize>,
    ) -> Option<TaskChoice> {
        crate::choose_process_task(choose_index)
    }

    unsafe fn switch_to_process(&mut self, choice: TaskChoice) -> bool {
        crate::switch_to_process(choice)
    }

    unsafe fn park_current_process(&mut self) {
        crate::park_current_process();
    }

    unsafe fn selected_process_task(&mut self) -> Option<PthreadT> {
        crate::selected_process_task()
    }

    unsafe fn fork(&mut self) -> libc::pid_t {
        crate::process_fork()
    }

    unsafe fn after_fork_child(&mut self) {
        crate::process_after_fork_child();
    }

    unsafe fn prepare_exec(&mut self) {
        crate::process_prepare_exec();
    }
}

pub(crate) fn default_task_provider() -> Box<dyn TaskProvider> {
    let local: Box<dyn TaskProvider> = if cfg!(feature = "coro") {
        Box::new(CoroTaskProvider::new())
    } else {
        Box::new(ThreadTaskProvider::new())
    };
    Box::new(ProcessTaskProvider::new(local))
}
