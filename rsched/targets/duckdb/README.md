# DuckDB target

An in-process stress test of DuckDB (submodule `duckdb`, v1.5.6) run under
rsched with the instrumented glibc. Only atomics are instrumented in DuckDB
itself; pthread and I/O calls are intercepted inside the instrumented libc.

```sh
# Once: the instrumented glibc (native provider) and DuckDB (~25 min on 2 cores).
cargo xtask build libc glibc --provider native
targets/duckdb/build.sh

targets/duckdb/run.sh native                       # ~0.4 s, 5 s timeout
SCALE=0.05 targets/duckdb/run.sh rsched 0 1 2 3    # one run per seed, 120 s timeout

# The coro provider: build it, then point run.sh at its manifest.
cargo xtask build libc glibc --provider coro
GLIBC_ARTIFACT=$(ls target/rsched-artifacts/glibc/coro/release/*/manifest.json) \
  SCALE=0.05 targets/duckdb/run.sh rsched 0
```

`stress.cpp` runs appenders, transactional inserts that commit or roll back,
conflicting updates of one hot row, checkpoints, parallel aggregates, joins,
sorts, and window functions, and an interrupted query, all against one
file-backed database with `DB_THREADS` (default 4) scheduler threads. It checks
every determined result and exits non-zero on a mismatch. `SCALE` multiplies
the workload; rsched runs need a fraction because every atomic is a scheduling
point.

Build pieces:

- `rsched-cc.sh` is the CMake compiler launcher that runs
  `opt -passes=rsched-atomics` between emit-llvm and codegen.
- `rsched_stub.c` defines a no-op `rsched_atomic_instrument` so the same
  binary runs natively; under rsched the preloaded libc's definition wins.
- `compat.c` supplies the glibc 2.36+ symbols that host headers and the
  static libstdc++ reference, so the binary loads under glibc 2.35.
