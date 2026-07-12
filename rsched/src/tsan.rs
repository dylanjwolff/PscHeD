#[cfg(feature = "tsan")]
use crate::{ParkingHandle, StartRoutine};
#[cfg(feature = "tsan")]
use std::ptr::addr_of_mut;

#[cfg(feature = "tsan")]
struct TSanGateArg {
    routine: StartRoutine,
    arg: *mut libc::c_void,
}

#[cfg(feature = "tsan")]
// SAFETY: The gate only transfers an opaque user thread-start function pointer
// and argument pointer to the newly created OS thread. Ownership of the boxed
// gate moves exactly once through `Box::into_raw`/`Box::from_raw`.
unsafe impl Send for TSanGateArg {}

pub(crate) unsafe fn sync_acquire() {
    ignore_begin();
}

pub(crate) unsafe fn sync_release() {
    ignore_end();
}

pub(crate) unsafe fn user_acquire(addr: *mut libc::c_void) {
    ignore_end();
    acquire(addr);
    ignore_begin();
}

pub(crate) unsafe fn user_release(addr: *mut libc::c_void) {
    ignore_end();
    release(addr);
    ignore_begin();
}

#[cfg(feature = "tsan")]
pub(crate) unsafe fn is_thread_start(start: StartRoutine, arg: *mut libc::c_void) -> bool {
    if arg.is_null() {
        return false;
    }

    let code = std::slice::from_raw_parts(start as *const u8, TSAN_THREAD_START_PREFIX.len());
    code == TSAN_THREAD_START_PREFIX
}

#[cfg(feature = "tsan")]
pub(crate) unsafe fn is_background_start(start: StartRoutine, arg: *mut libc::c_void) -> bool {
    if !arg.is_null() {
        return false;
    }

    let code = std::slice::from_raw_parts(start as *const u8, TSAN_BACKGROUND_START_PREFIX.len());
    code == TSAN_BACKGROUND_START_PREFIX
}

#[cfg(feature = "tsan")]
pub(crate) unsafe fn prepare_start_gate(arg: *mut libc::c_void) -> *mut libc::c_void {
    let fields = arg as *mut usize;
    let routine = std::mem::transmute_copy::<usize, StartRoutine>(&*fields);
    let user_arg = *fields.add(1) as *mut libc::c_void;
    let gate = Box::into_raw(Box::new(TSanGateArg {
        routine,
        arg: user_arg,
    }));
    *fields = user_start_gate as *const () as usize;
    *fields.add(1) = gate as usize;
    gate.cast()
}

#[cfg(feature = "tsan")]
pub(crate) unsafe fn restore_start_gate(arg: *mut libc::c_void, gate: *mut libc::c_void) {
    let gate_box = Box::from_raw(gate.cast::<TSanGateArg>());
    let fields = arg as *mut usize;
    *fields = gate_box.routine as usize;
    *fields.add(1) = gate_box.arg as usize;
}

#[cfg(feature = "tsan")]
extern "C" fn user_start_gate(raw: *mut libc::c_void) -> *mut libc::c_void {
    // SAFETY: `raw` was produced by `prepare_start_gate` with
    // `Box::into_raw::<TSanGateArg>`. The gate is consumed exactly once by the
    // thread start trampoline before invoking the original user routine.
    unsafe {
        let gate = Box::from_raw(raw as *mut TSanGateArg);
        let routine = gate.routine;
        let arg = gate.arg;
        let self_pt = crate::my_pt();

        crate::depth_enter();
        crate::rsched_glock();
        if crate::st().info.contains_key(&self_pt) {
            crate::st().task(self_pt).set_waiting(true);
            let cond_ptr = addr_of_mut!(crate::st().t(self_pt).suspend_cond);
            crate::st().task_provider.park(ParkingHandle::new(cond_ptr));
            crate::st().task(self_pt).set_waiting(false);
        }
        crate::rsched_gunlock();
        crate::depth_exit();

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
unsafe fn acquire(addr: *mut libc::c_void) {
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
unsafe fn acquire(_addr: *mut libc::c_void) {}

#[cfg(feature = "tsan")]
unsafe fn release(addr: *mut libc::c_void) {
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
unsafe fn release(_addr: *mut libc::c_void) {}

#[cfg(feature = "tsan")]
unsafe fn ignore_begin() {
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
unsafe fn ignore_begin() {}

#[cfg(feature = "tsan")]
unsafe fn ignore_end() {
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
unsafe fn ignore_end() {}
