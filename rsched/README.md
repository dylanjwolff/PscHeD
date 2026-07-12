# rsched

`rsched` is a controlled concurrency testing framework and concurrent model
checker for C programs, written in Rust. It turns pthread operations, process
creation, atomics, and explicit yields into cooperative scheduling points so
that executions are deterministic and reproducible.

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

The instrumented libc covers synchronization operations implemented by libc.
To make application atomic instructions into scheduling points as well,
compile the application with the bundled LLVM plugin:

```sh
clang -fpass-plugin=/path/to/runtime/librsched_llvm_pass.so \
  -o my_program my_program.c
```

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
| `RSCHED_SECCOMP` | unset | Set to `1` on Linux x86_64 to trap unsupported raw synchronization syscalls |

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
require Rust nightly, Clang and LLVM 17, a C toolchain, musl tools, `gawk`,
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

## Unsafe code

`rsched` has a substantial unsafe surface because it interposes on C ABIs and
controls thread, process, and coroutine execution. Unsafe blocks should have a
`SAFETY:` comment explaining the local invariant, and pure-Rust unsafe helpers
should be covered by unit tests where Miri can execute them.

The broad remaining sources of unsafe code are:

- C ABI boundaries: exported `extern "C"` interposition functions receive raw
  libc pointers and must preserve pthread, semaphore, process, and atomic ABI
  behavior.
- FFI calls: libc, raw syscalls, TSan hooks, LLVM C APIs, `dlsym`, and dynamic
  loader/object lookup paths all require unsafe calls or raw symbols.
- Scheduler global state: the runtime owns singleton scheduler state,
  process-shared mappings, DFS shared state, and cross-thread/process task
  bookkeeping that cannot be represented directly with ordinary Rust borrows.
- Thread and coroutine machinery: stack ownership, FS-base handling, raw start
  routine arguments, clone/pthread lifecycle control, and coroutine context
  switching require manual invariants.
- Atomic instrumentation wrappers: instrumented code passes raw addresses that
  are interpreted as Rust atomic pointers for scheduling and memory-operation
  hooks.
- Test and benchmark fixtures: Rust tests and benches call C fixture entry
  points directly.

The main architectural opportunities to reduce unsafety are to keep C ABI
shims thin, route internal logic through typed safe wrappers, encapsulate global
scheduler/process state behind narrow guard APIs, and isolate LLVM C API usage
behind a small adapter layer.

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
