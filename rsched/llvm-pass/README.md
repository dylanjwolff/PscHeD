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
void rsched_atomic_instrument(const void *ptr, size_t size, unsigned access);
```

Pass names:

- `rsched-atomics`: instrument LLVM atomic instructions only.
- `rsched-atomics<direct-pthread>`: also rewrite known pthread calls to the
  `rsched_pthread_*` symbols for static/direct rsched linking.

The pass instruments LLVM `load atomic`, `store atomic`, `atomicrmw`, and
`cmpxchg` instructions. It does not replace the atomic instruction; it inserts a
cooperative scheduling point immediately before it.

For libFuzzer-style targets, the pass wraps `LLVMFuzzerTestOneInput` so each
input is executed under multiple rsched schedules. The original fuzzer entry
point is renamed, and the exported entry point calls the rsched runtime helper.
The input bytes are hashed into the base scheduler seed, then the seed is
incremented for each schedule. Set `RSCHED_FUZZ_SCHEDULES` to choose the number
of schedules per input; the default is `5`.
