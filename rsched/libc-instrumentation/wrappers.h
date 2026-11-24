/* Wrappers that instrument-gcc.sh adds to instrumented glibc objects.
 *
 * The script renames a glibc function to REAL with objcopy, writes one
 * macro call per public, hidden, or versioned name of it into a C file
 * that includes this header, and links the compiled file into the object.
 *
 * Every wrapped function takes at most six integer or pointer arguments,
 * so forwarding six longs passes them unchanged in the x86_64 argument
 * registers; the callee ignores the ones it does not take. Results are
 * passed through as a long: rax comes back untouched, so a caller of an int
 * or void function reads what the real function returned. The file includes
 * no glibc headers, so these declarations cannot conflict with the real
 * prototypes. */

_Bool rsched_try_enter(void);
void rsched_exit(void);
void rsched_activate_instrumented_libc(void);
void rsched_file_call_done(long nr, long ret, const long *args);
long rsched_libc_syscall(long nr, long a0, long a1, long a2, long a3, long a4,
                         long a5);

#define RSCHED_SIX_LONGS long a0, long a1, long a2, long a3, long a4, long a5
#define RSCHED_SIX_ARGS a0, a1, a2, a3, a4, a5

/* NAME runs TARGET, rsched's model of the function, when it is the
 * outermost wrapped call, and REAL otherwise, such as when rsched itself
 * calls into libc. ACTIVATE marks the instrumented libc ready first; hidden
 * __GI_* names that libc calls internally do not. */
#define WRAPPER(NAME, TARGET, REAL, ACTIVATE, VISIBILITY)                      \
    long TARGET(RSCHED_SIX_LONGS);                                             \
    long REAL(RSCHED_SIX_LONGS);                                               \
    __attribute__((visibility(VISIBILITY))) long NAME(RSCHED_SIX_LONGS) {      \
        if (ACTIVATE)                                                          \
            rsched_activate_instrumented_libc();                               \
        long ret = rsched_try_enter() ? TARGET(RSCHED_SIX_ARGS)                \
                                      : REAL(RSCHED_SIX_ARGS);                 \
        rsched_exit();                                                         \
        return ret;                                                            \
    }

/* Binds NAME, a WRAPPER, to a versioned symbol such as
 * "pthread_cond_wait@@GLIBC_2.3.2". */
#define WRAPPER_VERSION(NAME, VERSION) __asm__(".symver " #NAME "," VERSION);

/* NAME runs REAL, a glibc write, truncate, or stat function, then passes
 * its result and arguments to rsched for the virtual file timestamps (see
 * src/files.rs). NR is the syscall the function makes. TYPE is its return
 * type, so an int -1 reaches rsched sign-extended. */
#define FILE_HOOK(NAME, REAL, NR, TYPE, VISIBILITY)                            \
    TYPE REAL(RSCHED_SIX_LONGS);                                               \
    __attribute__((visibility(VISIBILITY))) TYPE NAME(RSCHED_SIX_LONGS) {      \
        const long args[6] = {RSCHED_SIX_ARGS};                                \
        TYPE ret = REAL(RSCHED_SIX_ARGS);                                      \
        rsched_file_call_done(NR, (long)ret, args);                            \
        return ret;                                                            \
    }

/* glibc's variadic syscall(nr, ...). Variadic callers pass the integer
 * arguments as for a fixed one, the seventh on the stack, so this reads
 * all six; extra ones are garbage the kernel ignores, as in glibc. */
#define SYSCALL_TRAMPOLINE()                                                   \
    long syscall(long nr, RSCHED_SIX_LONGS) {                                  \
        return rsched_libc_syscall(nr, RSCHED_SIX_ARGS);                       \
    }
