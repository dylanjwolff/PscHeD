/*
 * asan_no_uaf.c – safe multithreaded memory access; ASAN must report no errors.
 *
 * Same producer/consumer structure as asan_uaf.c, but a mutex ensures the
 * consumer finishes reading the allocation before the producer frees it.
 *
 * Build:  make asan           (from c-examples/)
 * Run:    ./asan_no_uaf  → exits 0, no ASAN warnings
 */
#ifdef RSCHED
#  include "rsched.h"
#endif
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static int             *shared_ptr = NULL;
static pthread_barrier_t barrier;
static pthread_mutex_t   mtx = PTHREAD_MUTEX_INITIALIZER;

static void *producer(void *arg)
{
    (void)arg;
    int *data = malloc(sizeof(int));
    *data = 42;
    shared_ptr = data;

    pthread_barrier_wait(&barrier);

    pthread_mutex_lock(&mtx);   /* wait for consumer to finish reading */
    free(data);
    shared_ptr = NULL;
    pthread_mutex_unlock(&mtx);
    return NULL;
}

static void *consumer(void *arg)
{
    (void)arg;
    pthread_barrier_wait(&barrier);

    pthread_mutex_lock(&mtx);   /* exclude producer's free while we read */
    int *local = shared_ptr;
    if (local)
        printf("val=%d\n", *local);
    pthread_mutex_unlock(&mtx);
    return NULL;
}

int main(void)
{
    pthread_t t1, t2;
    pthread_barrier_init(&barrier, NULL, 2);
    pthread_create(&t1, NULL, producer, NULL);
    pthread_create(&t2, NULL, consumer, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    return 0;
}
