/*
 * ubsan_no_ub.c – no undefined behaviour; UBSan must report no errors.
 *
 * Same structure as ubsan_ub.c but uses long to avoid overflow.
 *
 * Covered by: cargo test -p rsched --test sanitizers_static
 */
#ifdef RSCHED
#  include "rsched.h"
#endif
#include <pthread.h>
#include <stdio.h>
#include <limits.h>

static volatile long shared = INT_MAX;

static void *worker(void *arg)
{
    (void)arg;
    /* promotes to long before addition — no overflow */
    long y = shared + 1L;
    printf("y=%ld\n", y);
    return NULL;
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    return 0;
}
