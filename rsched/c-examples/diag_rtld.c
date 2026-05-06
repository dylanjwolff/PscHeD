/* Diagnostic: print where RTLD_NEXT resolves for each preload-intercepted symbol.
 * Build with:
 *   clang [-fsanitize=address] -o diag_rtld diag_rtld.c \
 *     -L../target/debug -lrsched_preload -Wl,-rpath,$(realpath ../target/debug)
 */
#include <stdio.h>

extern void rsched_diagnose_rtld_next(void);

int main(void) {
    rsched_diagnose_rtld_next();
    return 0;
}
