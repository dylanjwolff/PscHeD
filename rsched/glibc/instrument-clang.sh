#!/usr/bin/env bash
set -euo pipefail

: "${RSCHED_GLIBC_CC:=gcc}"

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

instrument=false
if [[ -n "$source_file" ]]; then
    case "$(basename "$source_file")" in
        pthread_create.c|pthread_join.c|pthread_exit.c|\
        pthread_mutexattr_init.c|pthread_mutexattr_settype.c|pthread_mutexattr_destroy.c|\
        pthread_mutex_init.c|pthread_mutex_destroy.c|pthread_mutex_lock.c|\
        pthread_mutex_trylock.c|pthread_mutex_unlock.c|pthread_cond_wait.c|\
        pthread_cond_signal.c|pthread_cond_broadcast.c|pthread_barrier_init.c|\
        pthread_barrier_wait.c|sched_yield.c)
            instrument=true
            ;;
    esac
fi

if ! $compile || ! $instrument || [[ -z "$output_file" || "$output_file" != *.os ]]; then
    exec "$RSCHED_GLIBC_CC" "$@"
fi

: "${RSCHED_LLVM_PLUGIN:?set RSCHED_LLVM_PLUGIN to librsched_llvm_pass.so}"

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
bc_args+=(-DNO_HIDDEN '-D__builtin_va_arg_pack()=0')

clang-17 -emit-llvm "${bc_args[@]}"
opt-17 \
    -load-pass-plugin "$RSCHED_LLVM_PLUGIN" \
    '-passes=rsched-atomics<glibc-libc>' \
    "$input_bc" \
    -o "$instrumented_bc"
clang-17 -c "$instrumented_bc" -o "$output_file"
case "$(basename "$source_file")" in
    pthread_cond_wait.c)
        objcopy \
            --redefine-sym __pthread_cond_broadcast=__GI___pthread_cond_broadcast \
            --redefine-sym __pthread_cond_signal=__GI___pthread_cond_signal \
            "$output_file"
        ;;
    pthread_create.c)
        objcopy \
            --redefine-sym __pthread_getattr_default_np=__GI___pthread_getattr_default_np \
            "$output_file"
        ;;
esac
