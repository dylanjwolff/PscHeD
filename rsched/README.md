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

### 2. Instrumented libc and LLVM pass

The `rsched-llvm-pass` crate builds an LLVM plugin that rewrites atomic instructions and optionally rewrites `pthread_*` calls at the IR level, so programs compiled with Clang can be instrumented without source changes.

```sh
cargo build --release -p rsched-llvm-pass           # produces target/release/librsched_llvm_pass.so

# Instrument application atomics. pthread interception is provided by an
# LLVM-instrumented glibc or musl shared library:
clang -fpass-plugin=/path/to/librsched_llvm_pass.so \
      -o my_prog my_prog.c

# Use the dynamic loader built with the instrumented libc. The test suites
# generate equivalent runner scripts automatically.
LD_PRELOAD=/path/to/instrumented/libc.so \
  /path/to/instrumented/ld-linux-x86-64.so.2 \
  --library-path /path/to/instrumented ./my_prog
```

The libc itself is built with `rsched-atomics<glibc-libc>` or
`rsched-atomics<musl-libc>`. These modes wrap libc's pthread implementations
and intercept thread creation at libc's clone boundary, so applications retain
their normal pthread ABI and TLS setup.

### 3. Binary Instrumentation (automatic atomic instrumentation)

An `e9patch` binary instrumentation pass is also included in this repository.

```
./binary-instrumentation/instrument.sh <path to your binary>
```

This will instrument your target binary and *all* dynamically linked dependencies into the `instrumented` directory.
From there you can run your program with:

```
LD_PRELOAD=/path/to/instrumented/libc.so \
  /path/to/instrumented/ld-linux-x86-64.so.2 \
  --library-path /path/to/instrumented:./instrumented \
  ./instrumented/my_prog.inst
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
# Prerequisites: Rust nightly, clang-17, llvm-17-dev, musl-tools,
# gawk, bison, x86_64-unknown-linux-musl Rust target (see Dockerfile)
cargo test --workspace
```

The workspace tests instrument glibc and musl with the rsched LLVM pass. They
run synchronization-focused libc tests and ordinary application binaries with
the resulting libc shared object in `LD_PRELOAD`. Initialize the repository's
submodules before running the suites outside Docker.

Or via Docker (runs the full test suite in an isolated environment):

```sh
docker buildx build -t  rsched .
docker run --security-opt seccomp=unconfined rsched
```
