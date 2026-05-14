# rsched

A Controlled Concurenct Testing framework and concurrent model checker for C programs, written in Rust.
`rsched` intercepts pthread and atomic operations and replaces them with cooperative scheduling points, allowing a test to deterministically explore different thread interleavings.
Running the same program under many schedules surfaces concurrency bugs *deterministically* that would be otherwise difficult to trigger and reproduce.

## How it works

At each scheduling point (pthread functions, atomic operations, `sched_yield`, etc.)  `rsched` suspends the current thread and then chooses the next thread to run based on a *scheduling algorithm*.
These scheduling algorithms can be randomized (for bug finding) or exhaustive (for verification of small datastructures).
Even with the randomized scheduling algorithms, however, each run is seeded, so re-running the same seed reproduces the same interleaving.

## Integration modes and Usage

### 1. Static linking (`librsched.a` + `rsched.h`)

Add `rsched.h` to your source and compile against `librsched.a`. The header `#define`-redirects all pthread calls to their `rsched_*` counterparts at compile time; no runtime loader tricks are needed.

```c
#include "rsched.h"  // redirects pthread_create, mutex_lock, barrier_wait, …

// Optional: seed the scheduler explicitly before spawning threads.
// Without this,  `rsched` auto-initialises on first use.
rsched_reinit(seed);
```

Build and link:
```sh
cargo build --release --lib                          # produces target/release/librsched.a
clang -DRSCHED -Iinclude -o my_prog my_prog.c \
    -Wl,--start-group target/release/librsched.a -Wl,--end-group \
    -lpthread -ldl -lm
```

For atomic operations, use `rsched_atomic.h` instead of `<stdatomic.h>`. It provides the same `atomic_load_explicit` / `atomic_store_explicit` / `atomic_fetch_add_explicit` / … macros, each inserting a scheduling point before the access.

```c
#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <stdatomic.h>
#  include <pthread.h>
#endif
```

### 2. `LD_PRELOAD` (no recompilation, missing atomic instrumentation!)

Build the preload shim and inject it into an unmodified binary at run time:

```sh
cargo build --release -p rsched-preload             # produces target/release/librsched_preload.so
LD_PRELOAD=/path/to/librsched_preload.so RANDOM_SEED=42 ./my_program
```

Every `pthread_*` / `sched_yield` call in the target is silently redirected to rsched. Atomic operations are **not** intercepted by the preload shim (they require source-level instrumentation or the LLVM pass).

### 3. LLVM pass (automatic atomic instrumentation)

The `rsched-llvm-pass` crate builds an LLVM plugin that rewrites atomic instructions and optionally rewrites `pthread_*` calls at the IR level, so programs compiled with Clang can be instrumented without source changes.

```sh
cargo build --release -p rsched-llvm-pass           # produces target/release/librsched_llvm_pass.so

# Instrument atomics and use the preload shim for pthread interception:
clang -fpass-plugin=/path/to/librsched_llvm_pass.so \
      -o my_prog my_prog.c
LD_PRELOAD=/path/to/librsched_preload.so ./my_prog

# Or rewrite pthread calls directly in IR (no preload needed):
clang -fpass-plugin=".../librsched_llvm_pass.so=rsched-atomics<direct-pthread>" \
      -o my_prog my_prog.c -L... -lrsched ...
```

### 4. Binary Instrumentation (automatic atomic instrumentation)

An `e9patch` binary instrumentation pass is also included in this repository.

```
./binary-instrumentation/instrument.sh <path to your binary>
```

This will instrument your target binary and *all* dynamically linked dependencies into the `instrumented` directory.
From there you can run your program with:

```
LD_PRELOAD=/path/to/librsched_preload.so LD_LIBRARY_PATH=./instrumented ./instrumented/my_prog.inst
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `RANDOM_SEED` | `0` | RNG seed passed to  `rsched` on initialisation |
| `RSCHED_LOG` | unset | Set to `1` to log every scheduling decision to stderr |
| `RSCHED_SECCOMP` | unset | Set to `1` to install a seccomp filter (Linux x86\_64) that traps raw `futex` and other sensitve syscalls issued outside rsched, turning accidental scheduler bypass into an immediate crash |

## Fuzzer integration

When using the LLVM pass, `rsched` provides a naive integration with fuzzing via libfuzzer. For each fuzzer test input, it executes `N` schedules (5 by default).

The pass injects a call to `rsched_fuzzer_test_one_input` wrapping any calls to `LLVMFuzzerTestOneInput`:

```c
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
    return rsched_fuzzer_test_one_input(data, size, my_test_callback);
}
```

## Sanitizer compatibility

`rsched` is intended to be compatible with AddressSanitizer, UBSan, and ThreadSanitizer. The TSAN build requires `--features tsan` and should not be used with static linking.

## Platform and libc

Linux x86\_64 is `glibc` the primary target. `musl` libc is intended to be supported as well via static linking against a `x86_64-unknown-linux-musl` build.

## Building and testing

```sh
# Prerequisites: Rust nightly, clang-17, llvm-17-dev, musl-tools (see Dockerfile)
cargo test --workspace
```

Or via Docker (runs the full test suite in an isolated environment):

```sh
docker buildx build -t  rsched .
docker run --security-opt seccomp=unconfined rsched
```
