/// Scheduling algorithm interface.
///
/// Implementations receive the current blocking state of every registered
/// thread and return the index of the thread that should run next.
/// Returning `None` means every thread is blocked — i.e. deadlock.
pub trait Scheduler {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize>;
}

// ── PRNG (xorshift64) ────────────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self { Self(if seed == 0 { 1 } else { seed }) }

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
        Self { rng: Rng::new(seed) }
    }
}

impl Scheduler for RandomWalk {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize> {
        let n = is_blocking.len();
        if n == 0 { return None; }
        let start = self.rng.usize_less_than(n);
        if !is_blocking[start] { return Some(start); }
        for i in 1..n {
            let idx = (start + i) % n;
            if !is_blocking[idx] { return Some(idx); }
        }
        None
    }
}
