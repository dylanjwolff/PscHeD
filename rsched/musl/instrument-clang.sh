#!/usr/bin/env bash
set -euo pipefail

: "${RSCHED_LLVM_PLUGIN:?set RSCHED_LLVM_PLUGIN to librsched_llvm_pass.so}"

compile=false
source_file=
output_file=
args=("$@")

for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        -c)
            compile=true
            ;;
        -o)
            output_file=${args[i + 1]}
            ((i += 1))
            ;;
        *.c)
            source_file=${args[i]}
            ;;
    esac
done

if ! $compile || [[ -z "$source_file" || -z "$output_file" ]]; then
    exec clang-17 "$@"
fi

mkdir -p "$(dirname "$output_file")"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
input_bc="$work_dir/input.bc"
instrumented_bc="$work_dir/instrumented.bc"

bc_args=()
for ((i = 0; i < ${#args[@]}; i++)); do
    if [[ "${args[i]}" == "-o" ]]; then
        bc_args+=(-o "$input_bc")
        ((i += 1))
    else
        bc_args+=("${args[i]}")
    fi
done

clang-17 -emit-llvm "${bc_args[@]}"
opt-17 \
    -load-pass-plugin "$RSCHED_LLVM_PLUGIN" \
    '-passes=rsched-atomics<musl-libc>' \
    "$input_bc" \
    -o "$instrumented_bc"
clang-17 -c "$instrumented_bc" -o "$output_file"
