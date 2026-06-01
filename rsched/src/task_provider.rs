use crate::{AttrT, BarrierT, CondT, MutexT, PthreadT, StartArg};

pub(crate) trait TaskProvider {
    unsafe fn create(
        &mut self,
        thread: *mut PthreadT,
        attr: *const AttrT,
        start_arg: *mut StartArg,
    ) -> libc::c_int;

    unsafe fn join(&mut self, thread: PthreadT, retval: *mut *mut libc::c_void) -> libc::c_int;

    unsafe fn signal(&mut self, cond: *mut CondT) -> libc::c_int;

    unsafe fn wait(&mut self, cond: *mut CondT);

    unsafe fn global_lock(&mut self);

    unsafe fn global_unlock(&mut self);

    unsafe fn mutex_lock(&mut self, lock: *mut MutexT) -> libc::c_int;

    unsafe fn mutex_unlock(&mut self, lock: *mut MutexT) -> libc::c_int;

    unsafe fn barrier_init(
        &mut self,
        barrier: *mut BarrierT,
        attr: *const libc::pthread_barrierattr_t,
        count: libc::c_uint,
    ) -> libc::c_int;

    fn task_tid(&self, _thread: PthreadT) -> libc::pid_t {
        0
    }

    fn drive_join_with_scheduler(&self) -> bool {
        false
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

    unsafe fn signal(&mut self, cond: *mut CondT) -> libc::c_int {
        crate::with_internal_depth(|| (crate::rpt().cond_signal)(cond))
    }

    unsafe fn wait(&mut self, cond: *mut CondT) {
        crate::thread_cond_wait(cond);
    }

    unsafe fn global_lock(&mut self) {
        crate::thread_glock();
    }

    unsafe fn global_unlock(&mut self) {
        crate::thread_gunlock();
    }

    unsafe fn mutex_lock(&mut self, lock: *mut MutexT) -> libc::c_int {
        crate::with_internal_depth(|| (crate::rpt().mutex_lock)(lock))
    }

    unsafe fn mutex_unlock(&mut self, lock: *mut MutexT) -> libc::c_int {
        crate::with_internal_depth(|| (crate::rpt().mutex_unlock)(lock))
    }

    unsafe fn barrier_init(
        &mut self,
        barrier: *mut BarrierT,
        attr: *const libc::pthread_barrierattr_t,
        count: libc::c_uint,
    ) -> libc::c_int {
        crate::with_internal_depth(|| (crate::rpt().barrier_init)(barrier, attr, count))
    }
}

mod coro {
    use super::TaskProvider;
    use crate::{AttrT, BarrierT, CondT, MutexT, PthreadT, StartArg};
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
            while self
                .tasks
                .get(&thread)
                .is_some_and(|task| task.retval.is_none())
            {
                let selected = self.requested_next.take().unwrap_or(thread);
                if self.tasks.contains_key(&selected) {
                    self.resume_task(selected);
                } else {
                    break;
                }
            }

            let Some(task) = self.tasks.remove(&thread) else {
                return 0;
            };
            if !retval.is_null() {
                *retval = task.retval.unwrap_or(core::ptr::null_mut());
            }
            core::mem::forget(task);
            0
        }

        unsafe fn signal(&mut self, _cond: *mut CondT) -> libc::c_int {
            0
        }

        unsafe fn wait(&mut self, _cond: *mut CondT) {
            Self::suspend_current();
        }

        unsafe fn global_lock(&mut self) {}

        unsafe fn global_unlock(&mut self) {}

        unsafe fn mutex_lock(&mut self, _lock: *mut MutexT) -> libc::c_int {
            0
        }

        unsafe fn mutex_unlock(&mut self, _lock: *mut MutexT) -> libc::c_int {
            0
        }

        unsafe fn barrier_init(
            &mut self,
            _barrier: *mut BarrierT,
            _attr: *const libc::pthread_barrierattr_t,
            _count: libc::c_uint,
        ) -> libc::c_int {
            0
        }

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

        fn drive_join_with_scheduler(&self) -> bool {
            true
        }
    }
}

pub(crate) use coro::CoroTaskProvider;

pub(crate) fn default_task_provider() -> Box<dyn TaskProvider> {
    if cfg!(feature = "coro") {
        Box::new(CoroTaskProvider::new())
    } else {
        Box::new(ThreadTaskProvider::new())
    }
}
