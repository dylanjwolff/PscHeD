#!/usr/bin/env bash
set -euo pipefail

: "${RSCHED_GCC_PLUGIN:?set RSCHED_GCC_PLUGIN to rsched_gcc_pass.so}"
: "${RSCHED_WORKSPACE:?set RSCHED_WORKSPACE to the rsched workspace root}"

compiler=${RSCHED_GLIBC_CC:-gcc}
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

if ! $compile || [[ -z "$source_file" || -z "$output_file" ]]; then
    exec "$compiler" "$@"
fi

# The dynamic loader executes before libc and cannot call into rsched.
case "$(basename "$source_file"):$(basename "$output_file")" in
    rtld.c:*|dl-*.c:*|*:rtld-*)
        exec "$compiler" "$@"
        ;;
esac

if [[ "$(basename "$source_file")" == syscall.S ]]; then
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

case "$source_file" in
    *.S|*.s)
        exec "$compiler" "${args[@]}"
        ;;
esac

mkdir -p "$(dirname "$output_file")"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
object="$work_dir/input.o"
wrapped_object="$work_dir/wrapped.o"
wrapper_source="$work_dir/wrappers.S"
wrapper_object="$work_dir/wrappers.o"
symbols_file="$work_dir/symbols"

compile_args=()
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        -o)
            compile_args+=(-o "$object")
            ((i += 1))
            ;;
        *)
            compile_args+=("${args[i]}")
            ;;
    esac
done
compile_args+=("-fplugin=$RSCHED_GCC_PLUGIN")

"$compiler" "${compile_args[@]}"

nm -a --defined-only "$object" 2>/dev/null | awk '{ print $NF }' | sort -u >"$symbols_file"

symbol_defined() {
    grep -Fxq "$1" "$symbols_file"
}

emit_wrapper() {
    local symbol=$1
    local target=$2
    local real=$3
    local activate=$4
    local hidden=$5

    {
        printf '\n.text\n'
        printf '.globl %s\n' "$symbol"
        if [[ "$hidden" == yes ]]; then
            printf '.hidden %s\n' "$symbol"
        fi
        printf '.type %s, @function\n' "$symbol"
        printf '%s:\n' "$symbol"
        printf '    pushq %%rdi\n'
        printf '    pushq %%rsi\n'
        printf '    pushq %%rdx\n'
        printf '    pushq %%rcx\n'
        printf '    pushq %%r8\n'
        printf '    pushq %%r9\n'
        printf '    subq $8, %%rsp\n'
        if [[ "$activate" == yes ]]; then
            printf '    call rsched_activate_instrumented_libc@PLT\n'
        fi
        printf '    call rsched_try_enter@PLT\n'
        printf '    addq $8, %%rsp\n'
        printf '    popq %%r9\n'
        printf '    popq %%r8\n'
        printf '    popq %%rcx\n'
        printf '    popq %%rdx\n'
        printf '    popq %%rsi\n'
        printf '    popq %%rdi\n'
        printf '    testb %%al, %%al\n'
        printf '    je .Lrsched_nested_%s\n' "$symbol"
        printf '    subq $8, %%rsp\n'
        printf '    call %s@PLT\n' "$target"
        printf '    addq $8, %%rsp\n'
        printf '    pushq %%rax\n'
        printf '    pushq %%rdx\n'
        printf '    subq $8, %%rsp\n'
        printf '    call rsched_exit@PLT\n'
        printf '    addq $8, %%rsp\n'
        printf '    popq %%rdx\n'
        printf '    popq %%rax\n'
        printf '    ret\n'
        printf '.Lrsched_nested_%s:\n' "$symbol"
        printf '    jmp %s\n' "$real"
        printf '.size %s, .-%s\n' "$symbol" "$symbol"
    } >>"$wrapper_source"
}

emit_syscall_trampoline() {
    {
        printf '\n.text\n'
        printf '.globl syscall\n'
        printf '.type syscall, @function\n'
        printf 'syscall:\n'
        printf '    jmp rsched_libc_syscall@PLT\n'
        printf '.size syscall, .-syscall\n'
    } >>"$wrapper_source"
}

add_rewrite() {
    local impl=$1
    local public=$2
    local real=$3
    local rsched=$4
    local hidden=$5

    if ! symbol_defined "$impl"; then
        return
    fi

    objcopy_args+=(--redefine-sym "$impl=$real")
    if [[ "$public" != "$impl" ]] && symbol_defined "$public"; then
        objcopy_args+=(--redefine-sym "$public=__rsched_real_alias_$public")
        emit_wrapper "$public" "$rsched" "$real" yes no
    fi
    if [[ -n "$hidden" ]]; then
        if symbol_defined "$hidden"; then
            objcopy_args+=(--redefine-sym "$hidden=__rsched_real_alias_$hidden")
        fi
        emit_wrapper "$hidden" "$rsched" "$real" no yes
    fi
    emit_wrapper "$impl" "$rsched" "$real" yes no
}

objcopy_args=()
: >"$wrapper_source"

if symbol_defined syscall; then
    objcopy_args+=(--redefine-sym "syscall=__rsched_replaced_syscall")
    emit_syscall_trampoline
fi

add_rewrite "__pthread_create_2_1" "pthread_create" "__rsched_real_pthread_create" "rsched_pthread_create" "__GI___pthread_create"
add_rewrite "___pthread_join" "pthread_join" "__rsched_real_pthread_join" "rsched_pthread_join" "__GI___pthread_join"
add_rewrite "__pthread_exit" "pthread_exit" "__rsched_real_pthread_exit" "rsched_pthread_exit" "__GI___pthread_exit"
add_rewrite "___pthread_mutexattr_init" "pthread_mutexattr_init" "__rsched_real_pthread_mutexattr_init" "rsched_pthread_mutexattr_init" "__GI___pthread_mutexattr_init"
add_rewrite "___pthread_mutexattr_settype" "pthread_mutexattr_settype" "__rsched_real_pthread_mutexattr_settype" "rsched_pthread_mutexattr_settype" "__GI___pthread_mutexattr_settype"
add_rewrite "___pthread_mutexattr_destroy" "pthread_mutexattr_destroy" "__rsched_real_pthread_mutexattr_destroy" "rsched_pthread_mutexattr_destroy" "__GI___pthread_mutexattr_destroy"
add_rewrite "___pthread_mutex_init" "pthread_mutex_init" "__rsched_real_pthread_mutex_init" "rsched_pthread_mutex_init" "__GI___pthread_mutex_init"
add_rewrite "___pthread_mutex_destroy" "pthread_mutex_destroy" "__rsched_real_pthread_mutex_destroy" "rsched_pthread_mutex_destroy" "__GI___pthread_mutex_destroy"
add_rewrite "___pthread_mutex_lock" "pthread_mutex_lock" "__rsched_real_pthread_mutex_lock" "rsched_pthread_mutex_lock" "__GI___pthread_mutex_lock"
add_rewrite "___pthread_mutex_trylock" "pthread_mutex_trylock" "__rsched_real_pthread_mutex_trylock" "rsched_pthread_mutex_trylock" "__GI___pthread_mutex_trylock"
add_rewrite "___pthread_mutex_unlock" "pthread_mutex_unlock" "__rsched_real_pthread_mutex_unlock" "rsched_pthread_mutex_unlock" "__GI___pthread_mutex_unlock"
add_rewrite "___pthread_cond_wait" "pthread_cond_wait" "__rsched_real_pthread_cond_wait" "rsched_pthread_cond_wait" "__GI___pthread_cond_wait"
add_rewrite "___pthread_cond_signal" "pthread_cond_signal" "__rsched_real_pthread_cond_signal" "rsched_pthread_cond_signal" "__GI___pthread_cond_signal"
add_rewrite "___pthread_cond_broadcast" "pthread_cond_broadcast" "__rsched_real_pthread_cond_broadcast" "rsched_pthread_cond_broadcast" "__GI___pthread_cond_broadcast"
add_rewrite "___pthread_barrier_init" "pthread_barrier_init" "__rsched_real_pthread_barrier_init" "rsched_pthread_barrier_init" "__GI___pthread_barrier_init"
add_rewrite "___pthread_barrier_wait" "pthread_barrier_wait" "__rsched_real_pthread_barrier_wait" "rsched_pthread_barrier_wait" "__GI___pthread_barrier_wait"
add_rewrite "__sched_yield" "sched_yield" "__rsched_real_sched_yield" "rsched_sched_yield" "__GI___sched_yield"
add_rewrite "_exit" "_exit" "__rsched_real_process_exit" "rsched_process_exit_status" "__GI__exit"
add_rewrite "__waitpid" "waitpid" "__rsched_real_waitpid" "rsched_waitpid" "__GI___waitpid"

if [[ ${#objcopy_args[@]} -eq 0 ]]; then
    mv "$object" "$output_file"
    exit 0
fi

objcopy "${objcopy_args[@]}" "$object" "$wrapped_object"
if [[ -s "$wrapper_source" ]]; then
    printf '\n.section .note.GNU-stack,"",@progbits\n' >>"$wrapper_source"
    "$compiler" -c -x assembler "$wrapper_source" -o "$wrapper_object"
    ld -r "$wrapped_object" "$wrapper_object" -o "$output_file"
else
    mv "$wrapped_object" "$output_file"
fi
