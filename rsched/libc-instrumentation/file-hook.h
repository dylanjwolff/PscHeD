/* File hooks for the virtual file timestamps (see src/files.rs).
 *
 * instrument-gcc.sh renames a glibc write, truncate, or stat function to
 * REAL, then defines one FILE_HOOK per public or hidden name of it. The hook
 * runs the real function and passes its result and arguments to rsched,
 * which stamps the written file or fixes the reported times.
 *
 * Every hooked function takes at most six integer or pointer arguments, so
 * forwarding six longs passes them unchanged in the x86_64 argument
 * registers; the real function ignores the ones it does not take. TYPE is
 * the function's return type: int results are widened with their sign, so
 * rsched sees -1 on failure. */

void rsched_file_call_done(long nr, long ret, const long *args);

#define FILE_HOOK(NAME, REAL, NR, TYPE, VISIBILITY)                            \
    TYPE REAL(long, long, long, long, long, long);                             \
    __attribute__((visibility(VISIBILITY))) TYPE NAME(long a0, long a1,        \
                                                      long a2, long a3,        \
                                                      long a4, long a5) {      \
        const long args[6] = {a0, a1, a2, a3, a4, a5};                         \
        TYPE ret = REAL(a0, a1, a2, a3, a4, a5);                               \
        rsched_file_call_done(NR, (long)ret, args);                            \
        return ret;                                                            \
    }
