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

# glibc assembles most syscall wrappers, such as dup2, from stdin
# (make-syscalls.sh). Those get wrappers too, but no plugin.
stdin_asm=false
if $compile && [[ -z "$source_file" && -n "$output_file" && " ${args[*]} " == *" - "* ]]; then
    stdin_asm=true
fi

if ! $compile || [[ -z "$output_file" ]] || { [[ -z "$source_file" ]] && ! $stdin_asm; }; then
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
if ! $stdin_asm; then
    compile_args+=("-fplugin=$RSCHED_GCC_PLUGIN")
fi

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
        printf '    subq $8, %%rsp\n'
        printf '    call %s\n' "$real"
        printf '    addq $8, %%rsp\n'
        printf '    pushq %%rax\n'
        printf '    pushq %%rdx\n'
        printf '    subq $8, %%rsp\n'
        printf '    call rsched_exit@PLT\n'
        printf '    addq $8, %%rsp\n'
        printf '    popq %%rdx\n'
        printf '    popq %%rax\n'
        printf '    ret\n'
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

add_versioned_public_wrappers() {
    local public=$1
    local rsched=$2
    local real=$3
    local index=0
    local versioned

    while IFS= read -r versioned; do
        local old_alias="__rsched_real_version_${public}_${index}"
        local wrapper="__rsched_public_version_${public}_${index}"
        objcopy_args+=(--redefine-sym "$versioned=$old_alias")
        emit_wrapper "$wrapper" "$rsched" "$real" yes no
        printf '.symver %s,%s\n' "$wrapper" "$versioned" >>"$wrapper_source"
        ((index += 1))
    done < <(grep -E "^${public}@@?[^@]+$" "$symbols_file" || true)
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
    add_versioned_public_wrappers "$public" "$rsched" "$real"
    if [[ -n "$hidden" ]]; then
        if symbol_defined "$hidden"; then
            objcopy_args+=(--redefine-sym "$hidden=__rsched_real_alias_$hidden")
        fi
        emit_wrapper "$hidden" "$rsched" "$real" no yes
    fi
    emit_wrapper "$impl" "$rsched" "$real" yes no
}

# File and socket operations wrap only the public symbol that applications
# call. glibc's own callers, such as stdio, use the hidden __GI_* names and
# keep the real implementation, so tasks never switch while libc holds an
# internal lock. These wrappers do not activate rsched: the dynamic loader
# and single-threaded startup perform I/O before any task exists.
add_io_rewrite() {
    local impl=$1
    local public=$2
    local rsched=$3

    if ! symbol_defined "$public"; then
        return
    fi
    if [[ "$impl" == "$public" ]]; then
        objcopy_args+=(--redefine-sym "$public=__rsched_real_$public")
        emit_wrapper "$public" "$rsched" "__rsched_real_$public" no no
    else
        objcopy_args+=(--redefine-sym "$public=__rsched_real_alias_$public")
        emit_wrapper "$public" "$rsched" "$impl" no no
    fi
}

# Calls rsched_file_call_done(nr, result, args) after the real function,
# so rsched can keep virtual file timestamps (see src/files.rs). Saves the
# six argument registers as the args array. `int` sign-extends a 32-bit
# result.
emit_file_hook() {
    local symbol=$1
    local real=$2
    local nr=$3
    local width=$4
    local hidden=$5

    {
        printf '\n.text\n'
        printf '.globl %s\n' "$symbol"
        if [[ "$hidden" == yes ]]; then
            printf '.hidden %s\n' "$symbol"
        fi
        printf '.type %s, @function\n' "$symbol"
        printf '%s:\n' "$symbol"
        printf '    pushq %%r9\n'
        printf '    pushq %%r8\n'
        printf '    pushq %%rcx\n'
        printf '    pushq %%rdx\n'
        printf '    pushq %%rsi\n'
        printf '    pushq %%rdi\n'
        printf '    subq $8, %%rsp\n'
        printf '    call %s\n' "$real"
        if [[ "$width" == int ]]; then
            printf '    movslq %%eax, %%rax\n'
        fi
        printf '    movq %%rax, (%%rsp)\n'
        printf '    movq $%s, %%rdi\n' "$nr"
        printf '    movq %%rax, %%rsi\n'
        printf '    leaq 8(%%rsp), %%rdx\n'
        printf '    call rsched_file_call_done@PLT\n'
        printf '    movq (%%rsp), %%rax\n'
        printf '    addq $56, %%rsp\n'
        printf '    ret\n'
        printf '.size %s, .-%s\n' "$symbol" "$symbol"
    } >>"$wrapper_source"
}

# Hooks `impl` and each defined alias of it, including glibc's hidden
# __GI_* names that stdio and the stat functions call, so writes and stat
# results that libc makes internally reach the file timestamp model too.
add_file_hook() {
    local impl=$1
    local nr=$2
    local width=$3
    shift 3

    if ! symbol_defined "$impl"; then
        return
    fi
    local real="__rsched_real_file_$impl"
    objcopy_args+=(--redefine-sym "$impl=$real")
    emit_file_hook "$impl" "$real" "$nr" "$width" no
    local name hidden
    for name in "$@"; do
        if ! symbol_defined "$name"; then
            continue
        fi
        objcopy_args+=(--redefine-sym "$name=__rsched_real_alias_$name")
        hidden=no
        [[ "$name" == __GI_* ]] && hidden=yes
        emit_file_hook "$name" "$real" "$nr" "$width" "$hidden"
    done
}

objcopy_args=()
: >"$wrapper_source"

if symbol_defined syscall; then
    objcopy_args+=(--redefine-sym "syscall=__rsched_replaced_syscall")
    emit_syscall_trampoline
fi

add_rewrite "__pthread_create_2_1" "pthread_create" "__rsched_real_pthread_create" "rsched_pthread_create" "__GI___pthread_create"
add_rewrite "___pthread_join" "pthread_join" "__rsched_real_pthread_join" "rsched_pthread_join" "__GI___pthread_join"
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
add_rewrite "___pthread_cond_timedwait" "pthread_cond_timedwait" "__rsched_real_pthread_cond_timedwait" "rsched_pthread_cond_timedwait" "__GI___pthread_cond_timedwait"
add_rewrite "___pthread_cond_clockwait" "pthread_cond_clockwait" "__rsched_real_pthread_cond_clockwait" "rsched_pthread_cond_clockwait" "__GI___pthread_cond_clockwait"
# Semaphores, rwlocks, once, and sleeps wrap only their public symbols:
# libc's internal locks (__libc_rwlock_*, __libc_once) keep the real
# implementation, which never shares an object with application calls.
add_rewrite "__new_sem_wait" "sem_wait" "__rsched_real_sem_wait" "rsched_sem_wait" ""
add_rewrite "__new_sem_trywait" "sem_trywait" "__rsched_real_sem_trywait" "rsched_sem_trywait" ""
add_rewrite "___sem_timedwait" "sem_timedwait" "__rsched_real_sem_timedwait" "rsched_sem_timedwait" ""
add_rewrite "___sem_clockwait" "sem_clockwait" "__rsched_real_sem_clockwait" "rsched_sem_clockwait" ""
add_rewrite "__new_sem_post" "sem_post" "__rsched_real_sem_post" "rsched_sem_post" ""
add_rewrite "___pthread_rwlock_rdlock" "pthread_rwlock_rdlock" "__rsched_real_pthread_rwlock_rdlock" "rsched_pthread_rwlock_rdlock" ""
add_rewrite "___pthread_rwlock_wrlock" "pthread_rwlock_wrlock" "__rsched_real_pthread_rwlock_wrlock" "rsched_pthread_rwlock_wrlock" ""
add_rewrite "___pthread_rwlock_tryrdlock" "pthread_rwlock_tryrdlock" "__rsched_real_pthread_rwlock_tryrdlock" "rsched_pthread_rwlock_tryrdlock" ""
add_rewrite "___pthread_rwlock_trywrlock" "pthread_rwlock_trywrlock" "__rsched_real_pthread_rwlock_trywrlock" "rsched_pthread_rwlock_trywrlock" ""
add_rewrite "___pthread_rwlock_timedrdlock" "pthread_rwlock_timedrdlock" "__rsched_real_pthread_rwlock_timedrdlock" "rsched_pthread_rwlock_timedrdlock" ""
add_rewrite "___pthread_rwlock_timedwrlock" "pthread_rwlock_timedwrlock" "__rsched_real_pthread_rwlock_timedwrlock" "rsched_pthread_rwlock_timedwrlock" ""
add_rewrite "___pthread_rwlock_clockrdlock" "pthread_rwlock_clockrdlock" "__rsched_real_pthread_rwlock_clockrdlock" "rsched_pthread_rwlock_clockrdlock" ""
add_rewrite "___pthread_rwlock_clockwrlock" "pthread_rwlock_clockwrlock" "__rsched_real_pthread_rwlock_clockwrlock" "rsched_pthread_rwlock_clockwrlock" ""
add_rewrite "___pthread_rwlock_unlock" "pthread_rwlock_unlock" "__rsched_real_pthread_rwlock_unlock" "rsched_pthread_rwlock_unlock" ""
add_rewrite "___pthread_once" "pthread_once" "__rsched_real_pthread_once" "rsched_pthread_once" ""
add_rewrite "__nanosleep" "nanosleep" "__rsched_real_nanosleep" "rsched_nanosleep" ""
add_rewrite "__clock_nanosleep" "clock_nanosleep" "__rsched_real_clock_nanosleep" "rsched_clock_nanosleep" ""
add_rewrite "usleep" "usleep" "__rsched_real_usleep" "rsched_usleep" ""
add_rewrite "__sleep" "sleep" "__rsched_real_sleep" "rsched_sleep" ""
# Deterministic inputs (see src/virt.rs). gettimeofday and time are IFUNCs
# into the vDSO and stay real.
add_rewrite "__clock_gettime" "clock_gettime" "__rsched_real_clock_gettime" "rsched_clock_gettime" ""
add_rewrite "time" "time" "__rsched_real_time" "rsched_time" ""
add_rewrite "__gettimeofday" "gettimeofday" "__rsched_real_gettimeofday" "rsched_gettimeofday" ""
add_rewrite "sched_getcpu" "sched_getcpu" "__rsched_real_sched_getcpu" "rsched_sched_getcpu" ""
add_rewrite "__getrandom" "getrandom" "__rsched_real_getrandom" "rsched_getrandom" ""
# Kernel waits with a timeout move virtual time by the time they block.
add_rewrite "__ppoll" "ppoll" "__rsched_real_ppoll" "rsched_ppoll" ""
add_rewrite "__select" "select" "__rsched_real_select" "rsched_select" ""
add_rewrite "__pselect" "pselect" "__rsched_real_pselect" "rsched_pselect" ""
add_rewrite "epoll_wait" "epoll_wait" "__rsched_real_epoll_wait" "rsched_epoll_wait" ""
add_rewrite "epoll_pwait" "epoll_pwait" "__rsched_real_epoll_pwait" "rsched_epoll_pwait" ""
add_rewrite "__sigtimedwait" "sigtimedwait" "__rsched_real_sigtimedwait" "rsched_sigtimedwait" ""
add_rewrite "___pthread_barrier_init" "pthread_barrier_init" "__rsched_real_pthread_barrier_init" "rsched_pthread_barrier_init" "__GI___pthread_barrier_init"
add_rewrite "___pthread_barrier_wait" "pthread_barrier_wait" "__rsched_real_pthread_barrier_wait" "rsched_pthread_barrier_wait" "__GI___pthread_barrier_wait"
add_rewrite "__sched_yield" "sched_yield" "__rsched_real_sched_yield" "rsched_sched_yield" "__GI___sched_yield"
add_rewrite "_exit" "_exit" "__rsched_real_process_exit" "rsched_process_exit_status" "__GI__exit"
add_rewrite "__waitpid" "waitpid" "__rsched_real_waitpid" "rsched_waitpid" "__GI___waitpid"
add_rewrite "__libc_fork" "fork" "__rsched_real_fork" "rsched_fork" "__GI___fork"
add_io_rewrite "__libc_read" "read" "rsched_read"
add_io_rewrite "__libc_write" "write" "rsched_write"
add_io_rewrite "__readv" "readv" "rsched_readv"
add_io_rewrite "__writev" "writev" "rsched_writev"
add_io_rewrite "__libc_recv" "recv" "rsched_recv"
add_io_rewrite "__libc_recvfrom" "recvfrom" "rsched_recvfrom"
add_io_rewrite "__libc_recvmsg" "recvmsg" "rsched_recvmsg"
add_io_rewrite "__libc_send" "send" "rsched_send"
add_io_rewrite "__libc_sendto" "sendto" "rsched_sendto"
add_io_rewrite "__libc_sendmsg" "sendmsg" "rsched_sendmsg"
add_io_rewrite "__libc_accept" "accept" "rsched_accept"
add_io_rewrite "accept4" "accept4" "rsched_accept4"
add_io_rewrite "__libc_connect" "connect" "rsched_connect"
add_io_rewrite "__poll" "poll" "rsched_poll"
add_io_rewrite "__close" "close" "rsched_close"
add_io_rewrite "__close_range" "close_range" "rsched_close_range"
add_io_rewrite "__closefrom" "closefrom" "rsched_closefrom"
add_io_rewrite "__dup2" "dup2" "rsched_dup2"
add_io_rewrite "__dup3" "dup3" "rsched_dup3"

# The public write and writev wrappers above call these implementations,
# so their writes are stamped here too.
add_file_hook "__libc_write" 1 long __GI___libc_write __GI___write __GI_write __write
add_file_hook "__write_nocancel" 1 long __GI___write_nocancel
add_file_hook "__libc_pwrite64" 18 long __libc_pwrite __GI___pwrite __GI___pwrite64 __pwrite64 pwrite pwrite64
add_file_hook "__writev" 20 long __GI___writev
add_file_hook "__ftruncate64" 77 int __ftruncate ftruncate ftruncate64
add_file_hook "__truncate64" 76 int truncate truncate64
add_file_hook "fallocate64" 285 int fallocate
add_file_hook "__fstatat64" 262 int __GI___fstatat __GI___fstatat64 __fstatat fstatat fstatat64
add_file_hook "statx" 332 int

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
