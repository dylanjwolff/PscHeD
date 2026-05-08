use libc::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

pub static MAIN_STARTED: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __libc_start_main(
    main: *const c_void,
    argc: c_int,
    argv: *mut *mut c_char,
    init: *const c_void,
    fini: *const c_void,
    rtld_fini: *const c_void,
    stack_end: *mut c_void,
) -> c_int {
    MAIN_STARTED.store(true, Ordering::SeqCst);
    // forward to real __libc_start_main
    let f: unsafe extern "C" fn(
        *const c_void, c_int, *mut *mut c_char, *const c_void, *const c_void, *const c_void, *mut c_void
    ) -> c_int = libc::dlsym(libc::RTLD_NEXT, b"__libc_start_main\0".as_ptr() as *const _) as _;
    f(main, argc, argv, init, fini, rtld_fini, stack_end)
}
