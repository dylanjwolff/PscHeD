/*
 * ubsan_ub.c – deliberate signed-integer overflow in a worker thread.
 * UBSan must report "signed integer overflow".
 *
 * Build:  make ubsan-pre       (from c-examples/)
 * Run:    ./ubsan_ub_pre → UBSan prints "signed integer overflow"
 */
#ifdef RSCHED
#  include "rsched.h"
#endif
#include <pthread.h>
#include <stdio.h>
#include <limits.h>

static volatile int shared = INT_MAX;

static void *worker(void *arg)
{
    (void)arg;
    /* signed integer overflow — undefined behaviour in C */
    int y = shared + 1;
    printf("y=%d\n", y);
    return NULL;
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    return 0;
}
