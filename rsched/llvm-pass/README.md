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

The default pass mode assumes the program links dynamically against
`librsched_preload.so`, so pthread symbols keep their normal names and the
preload crate intercepts them. Atomic LLVM IR instructions are instrumented by
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
