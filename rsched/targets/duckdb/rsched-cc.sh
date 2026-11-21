#!/usr/bin/env bash
# CMake compiler launcher that inserts rsched scheduling points before every
# LLVM atomic instruction. Invoked as `rsched-cc.sh <compiler> <args...>`.
#
# `clang -fpass-plugin=librsched_llvm_pass.so` alone does not run the pass:
# the plugin only registers a pipeline-parsing callback for `-passes=`, so a
# compile is split into emit-llvm, `opt -passes=rsched-atomics`, and codegen.
set -euo pipefail

: "${RSCHED_LLVM_PLUGIN:?set RSCHED_LLVM_PLUGIN to librsched_llvm_pass.so}"

compiler=$1
shift
args=("$@")
compile=false
output_file=
source_file=
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        -c) compile=true ;;
        -o) output_file=${args[i + 1]}; ((i += 1)) ;;
        *.c|*.cc|*.cpp|*.cxx) source_file=${args[i]} ;;
    esac
done

if ! $compile || [[ -z "$output_file" || -z "$source_file" ]]; then
    exec "$compiler" "${args[@]}"
fi

work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
bc_args=()
for ((i = 0; i < ${#args[@]}; i++)); do
    if [[ "${args[i]}" == -o ]]; then
        bc_args+=(-o "$work_dir/input.bc")
        ((i += 1))
    else
        bc_args+=("${args[i]}")
    fi
done

"$compiler" -emit-llvm "${bc_args[@]}"
opt-18 -load-pass-plugin "$RSCHED_LLVM_PLUGIN" -passes=rsched-atomics \
    "$work_dir/input.bc" -o "$work_dir/instrumented.bc"
"$compiler" -c -fPIC -Wno-unused-command-line-argument \
    "$work_dir/instrumented.bc" -o "$output_file"
