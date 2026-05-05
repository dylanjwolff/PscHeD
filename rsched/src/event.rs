/// Whether a memory operation reads, writes, or does both (e.g. compare-exchange,
/// fetch-add).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
    /// Atomic read-modify-write: compare_exchange, fetch_add, fetch_xor, etc.
    ReadWrite,
}

/// The kind of operation a thread is about to execute.
#[derive(Clone, Copy, Debug)]
pub enum EventKind {
    /// Atomic memory operation.
    MemOp {
        mem_addr: *const libc::c_void,
        size: usize,
        access: AccessKind,
    },
    /// Explicit cooperative yield (`sched_yield`).
    SchedYield,
    /// Spawning a new thread (`pthread_create`).
    ThreadCreate,
    /// Mutex acquisition attempt (`pthread_mutex_lock` / `trylock`).
    LockAcq { lock: *const libc::c_void },
    /// Mutex release (`pthread_mutex_unlock`).
    LockRel { lock: *const libc::c_void },
}

/// Describes what a thread is about to execute at the next scheduling point.
///
/// Set on the running thread just before each cooperative context switch and
/// cleared after the operation completes.  Scheduling algorithms can inspect
/// `Thread::next_event` during `choose()` to make informed decisions about
/// which thread to run next (e.g. PCT, directed scheduling).
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Address in the calling code that triggered this scheduling point.
    /// Obtained via the LLVM `returnaddress(0)` intrinsic — equivalent to
    /// `@returnAddress()` in Zig or `__builtin_return_address(0)` in C.
    /// Zero if the address could not be determined.
    pub instr_addr: u64,
    pub kind: EventKind,
}
