#!/usr/bin/env bash
set -euo pipefail

: "${RSCHED_LIBC_MODE:?set RSCHED_LIBC_MODE to glibc or musl}"
: "${RSCHED_LLVM_PLUGIN:?set RSCHED_LLVM_PLUGIN to librsched_llvm_pass.so}"
: "${RSCHED_WORKSPACE:?set RSCHED_WORKSPACE to the rsched workspace root}"

args=("$@")
compile=false
source_file=
output_file=

for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        -c)
            compile=true
            ;;
        -o)
            output_file=${args[i + 1]}
            ((i += 1))
            ;;
        *.c|*.S|*.s)
            source_file=${args[i]}
            ;;
    esac
done

if [[ "$RSCHED_LIBC_MODE" == glibc ]]; then
    fallback_compiler=${RSCHED_GLIBC_CC:-gcc}
else
    fallback_compiler=clang-17
fi

if ! $compile || [[ -z "$source_file" || -z "$output_file" ]]; then
    exec "$fallback_compiler" "$@"
fi

# glibc's dynamic loader runs before libc and therefore cannot call rsched
# hooks. Keep its rtld and dl-* translation units uninstrumented until the
# instrumentation runtime is self-contained.
if [[ "$RSCHED_LIBC_MODE" == glibc ]]; then
    case "$(basename "$source_file"):$(basename "$output_file")" in
        rtld.c:*|dl-*.c:*|*:rtld-*)
            exec "$fallback_compiler" "$@"
            ;;
    esac
fi

# glibc's x86_64 float128 implementation uses GCC's _Float128 language
# extension, which Clang 17 cannot parse. These math-only sources do not
# implement synchronization or task creation, so compile them with GCC.
if [[ "$RSCHED_LIBC_MODE" == glibc ]]; then
    case "$source_file:$output_file" in
        */sysdeps/ieee754/float128/*|*f128*.c:*)
            exec "$fallback_compiler" "$@"
            ;;
    esac
fi

# glibc's NPTL gai helper is a large GNU extern inline function. GCC inlines
# every call at -O2, but Clang leaves an undefined out-of-line call even with
# -fgnu89-inline. The helper ultimately calls the instrumented pthread entry
# points, so compiling this single resolver implementation with GCC preserves
# scheduling semantics.
if [[ "$RSCHED_LIBC_MODE" == glibc && "$(basename "$source_file")" == gai_misc.c ]]; then
    exec "$fallback_compiler" "$@"
fi

# A small set of glibc I/O and timer implementations compose hidden aliases
# through word-size and symbol-version macros. Clang canonicalizes those alias
# chains before the LLVM pass can recover the intended public/__GI names,
# leaving unresolved hidden references. They contain no rsched primitives.
if [[ "$RSCHED_LIBC_MODE" == glibc ]]; then
    case "$source_file" in
        */sysdeps/unix/sysv/linux/pread.c|\
        */sysdeps/unix/sysv/linux/pread64.c|\
        */sysdeps/unix/sysv/linux/pwrite.c|\
        */sysdeps/unix/sysv/linux/pwrite64.c|\
        */sysdeps/unix/sysv/linux/timer_create.c|\
        */sysdeps/unix/sysv/linux/timer_delete.c|\
        */sysdeps/unix/sysv/linux/timer_getoverr.c|\
        */sysdeps/unix/sysv/linux/timer_gettime.c|\
        */sysdeps/unix/sysv/linux/timer_settime.c)
            exec "$fallback_compiler" "$@"
            ;;
    esac
fi

# LLVM module passes cannot transform glibc's assembly syscall definition.
# Substitute a C marker whose definition the pass replaces with the x86_64
# trampoline. This is the only source-file-specific policy in the driver.
if [[ "$RSCHED_LIBC_MODE" == glibc && "$(basename "$source_file")" == syscall.S ]]; then
    source_file="$RSCHED_WORKSPACE/libc-instrumentation/syscall.c"
    replaced_args=()
    for arg in "${args[@]}"; do
        case "$arg" in
            *.S|*.s)
                replaced_args+=("$source_file")
                ;;
            *)
                replaced_args+=("$arg")
                ;;
        esac
    done
    args=("${replaced_args[@]}")
    args+=(-O2)
fi

# Assembly has no LLVM IR for a module pass to transform. Compile it normally;
# syscall.S was replaced with the C marker above and is the sole exception.
case "$source_file" in
    *.S|*.s)
        exec "$fallback_compiler" "${args[@]}"
        ;;
esac

mkdir -p "$(dirname "$output_file")"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
input_bc="$work_dir/input.bc"
instrumented_bc="$work_dir/instrumented.bc"

bc_args=()
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        -o)
            bc_args+=(-o "$input_bc")
            ((i += 1))
            ;;
        -fno-toplevel-reorder|-fno-section-anchors|-msse2avx)
            # GCC-only layout/code-generation controls rejected by Clang 17.
            ;;
        *)
            bc_args+=("${args[i]}")
            ;;
    esac
done

if [[ "$RSCHED_LIBC_MODE" == glibc ]]; then
    bc_args+=(
        -DNO_HIDDEN
        '-D__builtin_va_arg_pack()=0'
        -fheinous-gnu-extensions
        -include "$RSCHED_WORKSPACE/libc-instrumentation/glibc-clang-compat.h"
    )
fi

clang-17 -emit-llvm "${bc_args[@]}"
opt-17 \
    -load-pass-plugin "$RSCHED_LLVM_PLUGIN" \
    "-passes=rsched-atomics<$RSCHED_LIBC_MODE-libc>" \
    "$input_bc" \
    -o "$instrumented_bc"
clang-17 -c "$instrumented_bc" -o "$output_file"
