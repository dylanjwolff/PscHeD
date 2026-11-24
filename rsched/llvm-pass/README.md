# rsched LLVM pass

This crate builds an out-of-tree LLVM new-pass-manager plugin for instrumenting
large C/C++ projects without editing their source.

Build:

```sh
cargo build -p rsched-llvm-pass
```

Use with clang/opt from the matching LLVM version:

```sh
clang -fpass-plugin=target/debug/librsched_llvm_pass.so ...
```

The default pass mode instruments application atomic operations while leaving
pthread symbols unchanged. Run the resulting program with an LLVM-instrumented
glibc or musl build, which supplies the rsched runtime and intercepts pthread
operations from inside libc. Atomic LLVM IR instructions are instrumented by
inserting calls to:

```c
void rsched_atomic_instrument_ra(const void *call_site, const void *ptr, size_t size,
                                 unsigned access);
```

Each atomic calls a per-module internal thunk, `rsched.atomic.hook`, which
passes its return address (the atomic's call site) to that function. The
thunk references the runtime weakly and does nothing when no rsched runtime
is loaded, so instrumented programs link and run without one. The pass marks
each module it instruments and skips one that is already marked.

With `clang -fpass-plugin=...`, the plugin adds `rsched-atomics` at the
optimizer-last extension point, which runs at every `-O` level. The other
modes below are selected by name with `opt -passes=...`.

Pass names:

- `rsched-atomics`: instrument LLVM atomic instructions only.
- `rsched-atomics<direct-pthread>`: also rewrite known pthread, file, and
  socket calls (`read`, `send`, `accept`, `poll`, and so on) to their
  `rsched_*` symbols for static/direct rsched linking.
- `rsched-atomics<musl-libc>` and `rsched-atomics<glibc-libc>`: used when
  building the instrumented libcs. They wrap libc's own pthread, file, and
  socket implementations. In musl mode the pass also routes the `exit` syscall
  that ends a thread, and `__unmapself`, through `rsched_thread_exit` and
  `rsched_unmapself`, so the coro provider ends only the calling task.

The pass instruments LLVM `load atomic`, `store atomic`, `atomicrmw`, and
`cmpxchg` instructions. It does not replace the atomic instruction; it inserts a
cooperative scheduling point immediately before it.

For libFuzzer-style targets, the pass wraps `LLVMFuzzerTestOneInput` so each
input is executed under multiple rsched schedules. The original fuzzer entry
point is renamed, and the exported entry point calls the rsched runtime helper.
The input bytes are hashed into the base scheduler seed, then the seed is
incremented for each schedule. Set `RSCHED_FUZZ_SCHEDULES` to choose the number
of schedules per input; the default is `5`.
