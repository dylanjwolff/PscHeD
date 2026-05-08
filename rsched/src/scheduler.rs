use crate::event::{AccessKind, Event, EventKind};

/// Scheduling algorithm interface.
///
/// Implementations receive the current blocking state of every registered
/// thread and return the index of the thread that should run next.
/// Returning `None` means every thread is blocked — i.e. deadlock.
pub trait Scheduler {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize>;

    /// Called just before each scheduling decision with the event the current
    /// thread is about to execute.  Default implementation is a no-op.
    fn on_event(&mut self, _event: Option<&Event>) {}
}

// ── PRNG (xorshift64) ────────────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn usize_less_than(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

// ── Random-walk scheduler ────────────────────────────────────────────────────

/// Uniformly random non-blocking thread selection (random walk).
///
/// On each scheduling point a starting index is chosen uniformly at random;
/// if that thread is blocking the algorithm scans forward until it finds a
/// non-blocking one.  Returns `None` only when every thread is blocking.
pub struct RandomWalk {
    rng: Rng,
}

impl RandomWalk {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
        }
    }
}

impl Scheduler for RandomWalk {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize> {
        let n = is_blocking.len();
        if n == 0 {
            return None;
        }
        let start = self.rng.usize_less_than(n);
        if !is_blocking[start] {
            return Some(start);
        }
        for i in 1..n {
            let idx = (start + i) % n;
            if !is_blocking[idx] {
                return Some(idx);
            }
        }
        None
    }
}

// ── Logging scheduler ────────────────────────────────────────────────────────

/// Wraps any `Scheduler` and prints each scheduling event to stderr before
/// delegating to the inner scheduler.  Enable with `RSCHED_LOG=1`.
pub struct LoggingScheduler<S: Scheduler> {
    inner: S,
}

impl<S: Scheduler> LoggingScheduler<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Scheduler> Scheduler for LoggingScheduler<S> {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize> {
        self.inner.choose(is_blocking)
    }

    fn on_event(&mut self, event: Option<&Event>) {
        if let Some(ev) = event {
            match ev.kind {
                EventKind::ThreadCreate => {
                    eprintln!("[rsched] ThreadCreate @ 0x{:x}", ev.instr_addr);
                }
                EventKind::LockAcq { lock } => {
                    eprintln!(
                        "[rsched] LockAcq(lock=0x{:x}) @ 0x{:x}",
                        lock as usize, ev.instr_addr
                    );
                }
                EventKind::LockRel { lock } => {
                    eprintln!(
                        "[rsched] LockRel(lock=0x{:x}) @ 0x{:x}",
                        lock as usize, ev.instr_addr
                    );
                }
                EventKind::SchedYield => {
                    eprintln!("[rsched] SchedYield @ 0x{:x}", ev.instr_addr);
                }
                EventKind::MemOp {
                    mem_addr,
                    size,
                    access,
                } => {
                    let kind = match access {
                        AccessKind::Read => "R",
                        AccessKind::Write => "W",
                        AccessKind::ReadWrite => "RW",
                    };
                    eprintln!(
                        "[rsched] MemOp({kind}, mem=0x{:x}, size={size}) @ 0x{:x}",
                        mem_addr as usize, ev.instr_addr
                    );
                }
            }
        }
        self.inner.on_event(event);
    }
}
