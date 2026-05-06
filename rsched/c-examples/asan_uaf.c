/*
 * asan_uaf.c – deliberate heap-use-after-free in a multithreaded program.
 * ASAN must report a heap-use-after-free error.
 *
 * Two barriers enforce the exact interleaving that triggers the bug:
 *
 *   producer                         consumer
 *   ──────────────────────────────   ──────────────────────────────
 *   malloc + publish shared_ptr
 *   barrier_wait(b1)                 barrier_wait(b1)
 *                                    local = shared_ptr  (capture)
 *   barrier_wait(b2)                 barrier_wait(b2)
 *   free(data)          ← freed!     printf(*local)      ← UAF
 *
 * After b2 both threads proceed concurrently: producer frees the allocation
 * while consumer dereferences its (now-freed) local copy.  rsched's scheduler
 * picks one to run first; either way the consumer eventually reads freed memory.
 *
 * Build:  make asan           (from c-examples/)
 * Run:    ./asan_uaf  → ASAN prints "heap-use-after-free"
 */
#ifdef RSCHED
#  include "rsched.h"
#endif
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static int              *shared_ptr = NULL;
static pthread_barrier_t b1, b2;

static void *producer(void *arg)
{
    (void)arg;
    int *data = malloc(sizeof(int));
    *data = 42;
    shared_ptr = data;

    pthread_barrier_wait(&b1);  /* consumer may now capture shared_ptr */
    pthread_barrier_wait(&b2);  /* consumer has captured; we may free   */

    free(data);                 /* free — consumer still holds 'local'  */
    shared_ptr = NULL;
    return NULL;
}

static void *consumer(void *arg)
{
    (void)arg;
    pthread_barrier_wait(&b1);  /* wait for pointer to be published */

    int *local = shared_ptr;    /* capture (valid at this point)    */

    pthread_barrier_wait(&b2);  /* signal producer: captured; it may free */

    /* heap-use-after-free: producer has freed 'local' */
    if (local)
        printf("val=%d\n", *local);
    return NULL;
}

int main(void)
{
    pthread_t t1, t2;
    pthread_barrier_init(&b1, NULL, 2);
    pthread_barrier_init(&b2, NULL, 2);
    pthread_create(&t1, NULL, producer, NULL);
    pthread_create(&t2, NULL, consumer, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    return 0;
}
