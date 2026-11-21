#!/usr/bin/env bash
# Builds DuckDB and the stress test with every LLVM atomic instrumented by the
# rsched pass. Output: $BUILD_DIR/duckdb_stress (default target/duckdb/build).
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
workspace=$(cd -- "$here/../.." && pwd)
build_dir=${BUILD_DIR:-"$workspace/target/duckdb/build"}
jobs=${JOBS:-$(nproc)}

export LLVM_SYS_180_PREFIX=${LLVM_SYS_180_PREFIX:-/usr/lib/llvm-18}
(cd "$workspace" && cargo build --release -p rsched-llvm-pass)
export RSCHED_LLVM_PLUGIN="$workspace/target/release/librsched_llvm_pass.so"

cmake -S "$here" -B "$build_dir" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_COMPILER=clang-18 \
    -DCMAKE_CXX_COMPILER=clang++-18 \
    -DCMAKE_C_COMPILER_LAUNCHER="$here/rsched-cc.sh" \
    -DCMAKE_CXX_COMPILER_LAUNCHER="$here/rsched-cc.sh" \
    -DBUILD_SHELL=OFF \
    -DBUILD_UNITTESTS=OFF \
    -DBUILD_EXTENSIONS=core_functions \
    -DSKIP_EXTENSIONS="parquet;jemalloc" \
    -DENABLE_SANITIZER=OFF \
    -DENABLE_UBSAN=OFF
# The launcher reads RSCHED_LLVM_PLUGIN from the environment at compile time.
cmake --build "$build_dir" --target duckdb_stress -j "$jobs"
echo "$build_dir/duckdb_stress"
