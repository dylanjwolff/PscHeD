use crate::event::{AccessKind, Event, EventKind};

pub(crate) const DFS_MAX_EVENTS: usize = 256;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct DfsPath {
    len: usize,
    choices: [u16; DFS_MAX_EVENTS],
}

impl DfsPath {
    const EMPTY: Self = Self {
        len: 0,
        choices: [0; DFS_MAX_EVENTS],
    };

    fn push(&mut self, choice: usize) {
        assert!(
            self.len < DFS_MAX_EVENTS,
            "rsched: dfs execution exceeded DFS_MAX_EVENTS"
        );
        assert!(
            choice <= u16::MAX as usize,
            "rsched: dfs choice ordinal does not fit in u16"
        );
        self.choices[self.len] = choice as u16;
        self.len += 1;
    }

    fn choice(&self, depth: usize) -> Option<usize> {
        (depth < self.len).then_some(self.choices[depth] as usize)
    }

    fn truncate(&mut self, len: usize) {
        assert!(len <= self.len, "rsched: invalid dfs path truncation");
        self.len = len;
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SharedDfsState {
    initialized: u8,
    in_progress: u8,
    has_next: u8,
    _pad: [u8; 5],
    completed: usize,
    path: DfsPath,
    replay_depth: usize,
    choices_at_depth: [u16; DFS_MAX_EVENTS],
}

impl SharedDfsState {
    pub(crate) const EMPTY: Self = Self {
        initialized: 0,
        in_progress: 0,
        has_next: 0,
        _pad: [0; 5],
        completed: 0,
        path: DfsPath::EMPTY,
        replay_depth: 0,
        choices_at_depth: [0; DFS_MAX_EVENTS],
    };

    fn reset(&mut self) {
        *self = Self::EMPTY;
        self.initialized = 1;
        self.has_next = 1;
    }

    fn ensure_initialized(&mut self) {
        if self.initialized == 0 {
            self.reset();
        }
    }

    fn start_execution_if_needed(&mut self) {
        self.ensure_initialized();
        if self.in_progress != 0 {
            return;
        }
        if self.has_next == 0 {
            return;
        }
        self.replay_depth = 0;
        self.choices_at_depth = [0; DFS_MAX_EVENTS];
        self.has_next = 0;
        self.in_progress = 1;
    }

    fn record_choice_count(&mut self, depth: usize, choices: usize) {
        assert!(
            depth < DFS_MAX_EVENTS,
            "rsched: dfs execution exceeded DFS_MAX_EVENTS"
        );
        assert!(
            choices <= u16::MAX as usize,
            "rsched: dfs choice count does not fit in u16"
        );
        self.choices_at_depth[depth] = choices as u16;
    }

    fn next_choice(&mut self, choices: usize) -> usize {
        let depth = self.replay_depth;
        self.record_choice_count(depth, choices);
        let choice = self.path.choice(depth).unwrap_or(0);
        assert!(
            choice < choices,
            "rsched: dfs path chose ordinal {choice}, but only {choices} tasks are runnable"
        );
        if depth == self.path.len {
            self.path.push(choice);
        }
        self.replay_depth += 1;
        choice
    }

    fn finish_execution(&mut self) {
        if self.in_progress == 0 {
            return;
        }
        self.completed += 1;
        self.in_progress = 0;

        for depth in (0..self.replay_depth).rev() {
            let choice = self.path.choices[depth] as usize;
            let choices = self.choices_at_depth[depth] as usize;
            if choice + 1 < choices {
                self.path.choices[depth] = (choice + 1) as u16;
                self.path.truncate(depth + 1);
                self.has_next = 1;
                return;
            }
        }
        self.has_next = 0;
        self.path = DfsPath::EMPTY;
    }
}

fn dfs_state() -> &'static mut SharedDfsState {
    // SAFETY: `shared_dfs_state` returns the process-shared DFS storage owned
    // by rsched. The pointer is validated below before it is dereferenced.
    let ptr = unsafe { crate::task_provider::shared_dfs_state() };
    debug_assert!(!ptr.is_null(), "rsched: DFS state pointer is null");
    debug_assert_eq!(
        (ptr as usize) % core::mem::align_of::<SharedDfsState>(),
        0,
        "rsched: DFS state pointer is misaligned"
    );
    // SAFETY: `shared_dfs_state` returns the process-shared scheduler state
    // backing DFS exploration. rsched serializes scheduler access through its
    // global scheduling lock, so callers do not alias this mutable reference.
    unsafe { &mut *ptr }
}

/// Scheduling algorithm interface.
///
/// Implementations receive the current blocking state of every registered
/// thread and return the index of the thread that should run next.
/// Returning `None` means every thread is blocked — i.e. deadlock.
pub trait Scheduler: Sized {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize>;

    /// Called just before each scheduling decision with the event the current
    /// thread is about to execute.  Default implementation is a no-op.
    fn on_event(&mut self, _event: Option<&Event>) {}

    /// Called when the current execution is complete and the scheduler can
    /// commit any run-local state.  Default implementation is a no-op.
    fn finish(&mut self) {}

    fn avoid_self_on_stutter(&self) -> bool {
        false
    }
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

// ── Depth-first scheduler ────────────────────────────────────────────────────

/// Deterministic exhaustive scheduler.
///
/// Each scheduling decision is represented by the ordinal of the selected
/// runnable task.  The shared state stores one path plus per-depth choice
/// counts; after each execution it backtracks to the deepest untried sibling.
pub struct DfsScheduler {
    should_branch: bool,
    finished: bool,
}

pub enum SchedulerImpl {
    Random(RandomWalk),
    Dfs(DfsScheduler),
    LoggingRandom(LoggingScheduler<RandomWalk>),
    LoggingDfs(LoggingScheduler<DfsScheduler>),
}

impl SchedulerImpl {
    pub fn new(seed: u64) -> Self {
        let logging = env_is("RSCHED_LOG", "1");
        let use_dfs = env_is("RSCHED_SCHEDULER", "dfs");
        match (use_dfs, logging) {
            (true, true) => Self::LoggingDfs(LoggingScheduler::new(DfsScheduler::new())),
            (true, false) => Self::Dfs(DfsScheduler::new()),
            (false, true) => Self::LoggingRandom(LoggingScheduler::new(RandomWalk::new(seed))),
            (false, false) => Self::Random(RandomWalk::new(seed)),
        }
    }
}

#[cfg(feature = "instrumented-libc")]
fn env_is(name: &str, value: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: `name` is a valid NUL-terminated C string. We immediately copy
    // from the returned pointer before any environment mutation.
    let ptr = unsafe { libc::getenv(name.as_ptr()) };
    if ptr.is_null() {
        return false;
    }

    // SAFETY: POSIX `getenv` returns either null or a pointer to a
    // NUL-terminated string owned by the process environment.
    let bytes = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes();
    bytes == value.as_bytes()
}

#[cfg(not(feature = "instrumented-libc"))]
fn env_is(name: &str, value: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == value)
}

impl Scheduler for SchedulerImpl {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize> {
        match self {
            Self::Random(scheduler) => scheduler.choose(is_blocking),
            Self::Dfs(scheduler) => scheduler.choose(is_blocking),
            Self::LoggingRandom(scheduler) => scheduler.choose(is_blocking),
            Self::LoggingDfs(scheduler) => scheduler.choose(is_blocking),
        }
    }

    fn on_event(&mut self, event: Option<&Event>) {
        match self {
            Self::Random(scheduler) => scheduler.on_event(event),
            Self::Dfs(scheduler) => scheduler.on_event(event),
            Self::LoggingRandom(scheduler) => scheduler.on_event(event),
            Self::LoggingDfs(scheduler) => scheduler.on_event(event),
        }
    }

    fn finish(&mut self) {
        match self {
            Self::Random(scheduler) => scheduler.finish(),
            Self::Dfs(scheduler) => scheduler.finish(),
            Self::LoggingRandom(scheduler) => scheduler.finish(),
            Self::LoggingDfs(scheduler) => scheduler.finish(),
        }
    }

    fn avoid_self_on_stutter(&self) -> bool {
        match self {
            Self::Random(scheduler) => scheduler.avoid_self_on_stutter(),
            Self::Dfs(scheduler) => scheduler.avoid_self_on_stutter(),
            Self::LoggingRandom(scheduler) => scheduler.avoid_self_on_stutter(),
            Self::LoggingDfs(scheduler) => scheduler.avoid_self_on_stutter(),
        }
    }
}

impl DfsScheduler {
    pub fn new() -> Self {
        dfs_state().start_execution_if_needed();
        Self {
            should_branch: false,
            finished: false,
        }
    }
}

impl Scheduler for DfsScheduler {
    fn choose(&mut self, is_blocking: &[bool]) -> Option<usize> {
        let runnable: Vec<usize> = is_blocking
            .iter()
            .enumerate()
            .filter_map(|(idx, blocking)| (!*blocking).then_some(idx))
            .collect();
        if runnable.is_empty() {
            return None;
        }

        if !self.should_branch || runnable.len() == 1 {
            return runnable.last().copied();
        }

        let state = dfs_state();
        let choice = state.next_choice(runnable.len());
        Some(runnable[choice])
    }

    fn on_event(&mut self, event: Option<&Event>) {
        self.should_branch = !matches!(
            event.map(|event| event.kind),
            None | Some(EventKind::SchedYield)
        );
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;

        dfs_state().finish_execution();
    }

    fn avoid_self_on_stutter(&self) -> bool {
        true
    }
}

pub fn dfs_reset() {
    dfs_state().reset();
}

pub fn dfs_has_next() -> bool {
    let state = dfs_state();
    state.ensure_initialized();
    state.has_next != 0
}

pub fn dfs_completed_schedules() -> usize {
    dfs_state().completed
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

    fn finish(&mut self) {
        self.inner.finish();
    }

    fn avoid_self_on_stutter(&self) -> bool {
        self.inner.avoid_self_on_stutter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dfs_state_enumerates_choices_without_repeating_completed_path() {
        let mut state = SharedDfsState::EMPTY;

        state.start_execution_if_needed();
        assert_eq!(state.next_choice(2), 0);
        assert_eq!(state.next_choice(3), 0);
        state.finish_execution();

        state.start_execution_if_needed();
        assert_eq!(state.next_choice(2), 0);
        assert_eq!(state.next_choice(3), 1);
        state.finish_execution();

        state.start_execution_if_needed();
        assert_eq!(state.next_choice(2), 0);
        assert_eq!(state.next_choice(3), 2);
        state.finish_execution();

        state.start_execution_if_needed();
        assert_eq!(state.next_choice(2), 1);
        assert_eq!(state.next_choice(3), 0);

        assert_eq!(state.completed, 3);
        assert_eq!(state.path.len, 2);
        assert_eq!(state.path.choice(0), Some(1));
        assert_eq!(state.path.choice(1), Some(0));
    }

    #[test]
    fn dfs_state_finishes_after_cartesian_product_is_exhausted() {
        let mut state = SharedDfsState::EMPTY;
        let mut visited = Vec::new();

        while {
            state.start_execution_if_needed();
            state.in_progress != 0
        } {
            let first = state.next_choice(2);
            let second = state.next_choice(2);
            visited.push((first, second));
            state.finish_execution();
        }

        assert_eq!(visited, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(state.completed, 4);
        assert_eq!(state.has_next, 0);
        assert_eq!(state.path.len, 0);
    }

    #[test]
    fn random_walk_zero_seed_is_deterministic_and_nonzero() {
        let mut a = RandomWalk::new(0);
        let mut b = RandomWalk::new(0);
        let runnable = [false, true, false, false];

        let choices_a: Vec<_> = (0..16).map(|_| a.choose(&runnable)).collect();
        let choices_b: Vec<_> = (0..16).map(|_| b.choose(&runnable)).collect();

        assert_eq!(choices_a, choices_b);
        assert!(choices_a.iter().all(Option::is_some));
    }
}
