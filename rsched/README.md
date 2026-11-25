# rsched

`rsched` is a controlled concurrency testing framework and concurrent model
checker for C programs, written in Rust. It turns pthread operations, process
creation, atomics, file and socket I/O, and explicit yields into cooperative
scheduling points so that executions are deterministic and reproducible.

The scheduler can either choose a seeded random execution or exhaustively
explore bounded executions using depth-first search (DFS).

## Supported integration modes

`rsched` supports two ways to run a C program:

1. Source-level annotations linked against `librsched.a`.
2. An unmodified pthread ABI provided by an instrumented glibc or musl,
   optionally combined with the LLVM pass for application atomics.

## Source-level integration

Include `rsched.h` to redirect supported pthread, process, and scheduling
operations to their `rsched_*` implementations:

```c
#include "rsched.h"

int main(void) {
    rsched_reinit(0);
    /* Create and synchronize tasks normally. */
}
```

Build and link against the static library:

```sh
clang -DRSCHED -I/path/to/runtime/include -o my_program my_program.c \
  -Wl,--start-group /path/to/runtime/librsched.a -Wl,--end-group \
  -lpthread -ldl -lm
```

For C11 atomics, include `rsched_atomic.h` instead of `<stdatomic.h>`. It
provides compatible atomic macros that insert scheduling points:

```c
#ifdef RSCHED
#include "rsched_atomic.h"
#else
#include <pthread.h>
#include <stdatomic.h>
#endif
```

The static release archive contains:

```text
runtime/librsched.a
runtime/librsched_llvm_pass.so
runtime/include/rsched.h
runtime/include/rsched_atomic.h
```

## Instrumented libc integration

The glibc and musl release archives contain an instrumented libc, its matching
dynamic loader, required runtime libraries, the LLVM pass, and a relocatable
launcher:

```text
runtime/run
runtime/librsched_llvm_pass.so
runtime/<dynamic-loader>
runtime/lib/<instrumented-libc>
```

Run an ordinary dynamically linked program through the bundle:

```sh
/path/to/runtime/run ./my_program arg1 arg2
```

The launcher selects the bundled dynamic loader and preloads the instrumented
libc. Applications retain the libc pthread ABI and normal libc thread-local
storage setup. Thread creation is intercepted at libc's clone boundary.
The preload goes to the loader's `--preload` option rather than
`LD_PRELOAD`, so programs the application executes, such as `/bin/sh` from
`system()`, run uninstrumented with the host libc. Forks without `exec` stay
instrumented and scheduled.

The instrumented libc covers synchronization operations implemented by libc.
To make application atomic instructions into scheduling points as well,
compile the application with the bundled LLVM plugin:

```sh
clang -fpass-plugin=/path/to/runtime/librsched_llvm_pass.so \
  -o my_program my_program.c
```

The plugin joins clang's pipeline at its last optimization step, at every
`-O` level, and inserts a call before each atomic instruction. That call
reaches rsched through a weak reference, so the same binary also links and
runs natively, where the calls do nothing. C++ works the same way; header
code such as `std::atomic` and `shared_ptr` is instrumented, while a
precompiled libstdc++ is not.

The instrumented glibc is 2.39, the version on Ubuntu 24.04, so programs
built against that host glibc or an older one load against it.

The LLVM pass has separate glibc and musl modes when it is used to build libc.
Application builds do not need to select those internal modes.

## Scheduling

The default scheduler uses a deterministic pseudorandom choice sequence.
Set the seed to reproduce an execution:

```sh
RANDOM_SEED=42 /path/to/runtime/run ./my_program
```

Select exhaustive DFS scheduling for bounded test cases:

```sh
RSCHED_SCHEDULER=dfs /path/to/runtime/run ./my_program
```

Source-level tests can also control DFS explicitly through
`rsched_dfs_reset`, `rsched_dfs_has_next`, and
`rsched_dfs_finish_current`.

Useful environment variables:

| Variable | Default | Description |
|---|---:|---|
| `RANDOM_SEED` | `0` | Seed for the random scheduler |
| `RSCHED_SCHEDULER` | random | Set to `dfs` for depth-first exploration |
| `RSCHED_LOG` | unset | Set to `1` to log scheduling decisions |
| `RSCHED_FUZZ_SCHEDULES` | `5` | Schedules executed for each libFuzzer input |
| `RSCHED_SECCOMP` | unset | Set to `1` on Linux x86_64 to trap unsupported raw synchronization syscalls and route I/O issued outside rsched through the I/O model |

## Synchronization and time

Mutexes, condition variables (including `pthread_cond_timedwait` and
`pthread_cond_clockwait`), barriers, semaphores (`sem_wait`, `sem_trywait`,
`sem_timedwait`, `sem_clockwait`, `sem_post`), reader-writer locks,
`pthread_once`, and sleeps (`nanosleep`, `clock_nanosleep`, `usleep`,
`sleep`) are scheduling points. A task blocked on one is not runnable until
another task releases it.

Deadlines pass in virtual time. A timed wait or sleep expires once the
virtual clock reaches its deadline. Every scheduling point moves the clock
forward, so a task that spins on a flag cannot starve a sleeper. When no task
can run, rsched picks one timed wait, moves the clock to its deadline, and
expires it. A timeout is never reported early, and which wait expires and
when depends only on the schedule.

`kill` (and `killpg`) between scheduled processes is a scheduling point.
rsched blocks signals in a thread while it parks it, so a signal runs only
once the thread holds the turn and returns to the program. The sender wakes
the target's tasks blocked on I/O or in a sleep; the woken task runs the
handler and its call fails with EINTR, or restarts under SA_RESTART (`poll`,
`epoll_wait`, and sleeps never restart). A signal whose default action ends
the process hands the schedule on first. Signals do not interrupt mutex,
semaphore, or condition variable waits, stop signals are not modeled, and
kernel-generated signals such as SIGCHLD arrive whenever the kernel sends
them (see `src/signals.rs`).

Calls that wait in the kernel and that rsched does not model, such as
`ppoll`, `select`, `pselect`, `epoll_pwait2`, and `sigtimedwait`, move the
virtual clock by one tick, and by their timeout when they report that it
passed. Time never depends on how long they
really blocked. A `select`, `pselect`, or `ppoll` that watches no
descriptor, such as `select(0, NULL, NULL, NULL, &tv)`, is a sleep: it
sleeps in virtual time like `nanosleep`.

So that one seed always takes the same path, programs also read time,
their CPU, and randomness from deterministic models, and run without address-space randomization:

- `clock_gettime` on the realtime, monotonic, and boot-time clocks returns
  virtual time. It starts at a fixed time, 2024-01-01T00:00:00Z for the
  realtime clock and 1000 s for the monotonic clocks, and advances
  by 1 µs at every scheduling point and every clock read, so a loop that
  spins on the clock still ends. `time` and `gettimeofday` read the same
  virtual realtime clock. CPU-time clocks stay real.
- File times follow virtual time. rsched keeps a table of the access,
  modification, and change times of files, keyed by device and inode, in
  memory shared by the process tree. A successful `write`, `pwrite`,
  `writev`, `pwritev`, `truncate`, `ftruncate`, or `fallocate` sets the
  file's mtime and ctime to the virtual time, and `stat`, `fstat`,
  `lstat`, `fstatat`, and `statx` report the table's times. A file seen
  for the first time reports the virtual epoch for every time. A change
  rsched does not see on a recorded file, such as a write through `mmap`,
  shows up as a different kernel time and is reported as the virtual time
  of the stat call that saw it. With instrumented glibc,
  stdio's internal writes are stamped too; with instrumented musl, only the
  public functions are.
- `sched_getcpu` reports a CPU derived from the calling task.
- Addresses stay fixed. Programs hash them, and a different hash takes a
  different number of atomic steps, so with ASLR one seed would schedule
  differently on every run. The instrumented-libc runners start the program
  under `setarch -R`, and warn when the host does not allow it; run
  source-mode programs under `setarch -R` yourself. Addresses also depend on
  the environment and arguments, so replay a seed with the same ones.
- `getrandom`, including `syscall(SYS_getrandom)` and, under
  `RSCHED_SECCOMP=1`, the raw syscall, returns bytes from a generator seeded
  by `RANDOM_SEED`. Reads of `/dev/urandom` are not modeled.

Semaphores must be shared within one process. Process-shared rwlocks and
`pthread_once` routines that unwind are not modeled; a routine that throws
aborts the process.

## File and socket I/O

`read`, `write`, `readv`, `writev`, `recv`, `recvfrom`, `recvmsg`, `send`,
`sendto`, `sendmsg`, `accept`, `accept4`, `connect`, `poll`, `epoll_wait`, and
`epoll_pwait` are scheduling points. Only one task runs at a time, so a call that blocked in the kernel
would stall the task that could unblock it. On a blocking descriptor that is
not ready, the calling task instead blocks, like a task waiting for a mutex:
it is not runnable, so it adds no scheduling points, until the descriptor is
ready, and then performs the call. When every task is blocked and some wait
on I/O, rsched waits in the kernel for one of their descriptors. A `poll`,
`epoll_wait`, or `epoll_pwait` with a positive timeout is a timed wait: it
blocks until a descriptor is ready or its deadline passes, which happens
once virtual time reaches it or no task can run, and then returns 0.

Each integration mode reaches these operations differently:

- Source annotations: `rsched.h` maps the calls to `rsched_*` wrappers, and
  `syscall()` routes the matching syscall numbers.
- Instrumented libc: the public glibc and musl functions and `syscall()` are
  wrapped. libc-internal I/O, such as stdio buffering, is not modeled.
- LLVM pass: `rsched-atomics<direct-pthread>` rewrites the calls to the
  `rsched_*` wrappers.
- Binary instrumentation: raw `syscall` instructions in the instrumented
  binary call into the model first.
- `RSCHED_SECCOMP=1`: I/O syscalls issued outside rsched, including stdio's,
  trap and block until the call can no longer block. They do not add
  scheduling points of their own, because libc may hold internal locks
  around them.

rsched keeps two descriptors open for itself, a memfd and an epoll, at
numbers of 900 or more when the descriptor limit allows. The program cannot
close or replace them: `close` of one reports success, `close_range` and
`closefrom` skip them, and `dup2` or `dup3` onto one fails with `EBADF`. Every
integration mode guards these calls the way it reaches I/O.

Writes are assumed to fit in the free buffer space once a descriptor reports
writable. `epoll_wait` and `epoll_pwait` wait on the epoll descriptor like a
`poll` on it, so a wait with a timeout expires in virtual time. `select`,
`ppoll`, `epoll_pwait2`, and opening FIFOs are not modeled.

## Processes

`fork`, `waitpid`, `_exit`, and `execv`/`execve` are scheduled: a forked child
runs as another task, and only one process runs at a time. Each integration
mode reaches `fork` differently:

- Source annotations: `rsched.h` maps `fork` to `rsched_fork`, and
  `syscall()` routes `SYS_fork` and process-style `SYS_clone`.
- Instrumented libc: the glibc and musl `fork` and `_exit` functions are
  wrapped, and libc's clone boundary routes process-style clones. `execve`
  is not wrapped, so a program the application executes leaves rsched; the
  bundled libc exports `rsched_execve`, which keeps the process in its
  rsched slot across the exec. Call it on the runner to start another
  instrumented program as part of the same execution, such as a driver
  that runs a server and its client as one process tree.
- LLVM pass: `rsched-atomics<direct-pthread>` rewrites `fork`, `waitpid`,
  `_exit`, `execv`, `execve`, and `syscall`.
- Binary instrumentation: a raw `fork` or process-style `clone` instruction
  runs through the scheduler instead.
- `RSCHED_SECCOMP=1`: a `fork` syscall issued outside rsched is trapped and
  run through the scheduler. With `RSCHED_SECCOMP_TRAP_CLONE=1`, process-style
  `clone` is too, while thread-style clones still abort.

Timed waits and sleeps expire across processes: when no task of any
process can run, the running process may expire a timed wait of another
process, so a parent sleeping between polls of its children, as Apache's
master does, wakes even while every child waits on I/O. A thread that exits
while the other threads of its process are blocked passes the turn to
another process and waits until a task of its own process is chosen again
before it exits.

`vfork` and `clone` with `CLONE_VM` are not modeled.

## Binary instrumentation

The repository includes an e9patch-based binary instrumentation tool for
inserting scheduling points into an already-built application and its shared
libraries:

```sh
./binary-instrumentation/instrument.sh /path/to/my_program
```

Run the resulting binary with the matching instrumented libc bundle. The
exact output paths are reported by the instrumentation script.

## Sanitizers and fuzzing

`rsched` is tested with AddressSanitizer, UBSan, and ThreadSanitizer.
ThreadSanitizer support requires the `tsan` Cargo feature and is intended for
the instrumented-libc mode rather than static linking.

When the LLVM pass sees `LLVMFuzzerTestOneInput`, it redirects execution
through `rsched_fuzzer_test_one_input` so that each input is evaluated under
multiple deterministic schedules.

## Workspace layout

```text
src/                         Scheduler and emulation runtime
include/                     Source-level C headers
llvm-pass/                   LLVM instrumentation plugin
libc-instrumentation/        Shared glibc/musl compiler driver and compatibility
crates/rsched-libc-build/    Cached artifact and libc build implementation
xtask/                       Developer build, test, and packaging commands
tests/source/                Source-level/static-library integration tests
tests/libc/                  Upstream glibc and musl conformance tests
tests/preload/               Unmodified-binary tests using instrumented libc
glibc/                       glibc source checkout
musl/                        musl source checkout
binary-instrumentation/      e9patch instrumentation support
```

The `tests/preload` name describes how those tests launch ordinary binaries:
they preload the instrumented glibc or musl. It does not refer to a separate
`rsched-preload` crate.

## Building artifacts

Prerequisites are documented by the repository `Dockerfile`. Native builds
require Rust nightly, Clang and LLVM 18, a C toolchain, musl tools, `gawk`,
`bison`, `zstd`, and the Rust `x86_64-unknown-linux-musl` target.

Use `xtask` to build artifacts:

```sh
# Static source-integration artifact.
cargo xtask build static --provider native

# Instrumented libc artifacts.
cargo xtask build libc glibc --provider coro
cargo xtask build libc musl --provider native

# Every artifact for both providers.
cargo xtask build all --provider all
```

`--provider native` uses native OS tasks. `--provider coro` uses coroutine
emulation. If omitted, both providers are built. The default profile is
`release`; pass `--profile debug` for development builds.

Artifacts are content-addressed under `target/rsched-artifacts`. An unchanged
rsched source tree, libc checkout, provider, profile, and toolchain reuse the
same cached build.

## Release packages

Build all release archives into `dist/`:

```sh
cargo xtask package all --provider all
```

This produces:

```text
rsched-static-native-x86_64-linux.tar.zst
rsched-static-coro-x86_64-linux.tar.zst
rsched-glibc-native-x86_64-linux.tar.zst
rsched-glibc-coro-x86_64-linux.tar.zst
rsched-musl-native-x86_64-linux.tar.zst
rsched-musl-coro-x86_64-linux.tar.zst
```

Each archive includes a `manifest.json` describing its provider, target,
fingerprint, and runtime paths.

## Testing

Initialize the glibc, musl, and binary-instrumentation submodules before
running the complete suite.

The test groups are intentionally separate:

```sh
# Source-level tests and LLVM pass tests, with both providers.
cargo xtask test source --provider all

# Upstream libc synchronization/conformance tests.
cargo xtask test libc glibc --provider native
cargo xtask test libc musl --provider coro

# Unmodified applications launched with an instrumented libc.
cargo xtask test preload glibc --provider native
cargo xtask test preload musl --provider coro
```

The libc conformance and preload suites consume the same cached libc artifact.
They do not maintain independent libc builds.

For an isolated source and LLVM-pass test environment:

```sh
docker buildx build -t rsched .
docker run --rm --security-opt seccomp=unconfined rsched
```

Linux x86_64 is currently the only supported platform.
