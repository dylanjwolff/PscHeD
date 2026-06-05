#if defined(CASE_ASAN_CLEAN) || defined(CASE_ASAN_UAF)

#ifdef RSCHED
#include "rsched.h"
#endif
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static int *shared_ptr = NULL;

#if defined(CASE_ASAN_CLEAN)

static pthread_barrier_t barrier;
static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;

static void *producer(void *arg) {
    (void)arg;
    int *data = malloc(sizeof(int));
    *data = 42;
    shared_ptr = data;

    pthread_barrier_wait(&barrier);

    pthread_mutex_lock(&mtx);
    free(data);
    shared_ptr = NULL;
    pthread_mutex_unlock(&mtx);
    return NULL;
}

static void *consumer(void *arg) {
    (void)arg;
    pthread_barrier_wait(&barrier);

    pthread_mutex_lock(&mtx);
    int *local = shared_ptr;
    if (local)
        printf("val=%d\n", *local);
    pthread_mutex_unlock(&mtx);
    return NULL;
}

int main(void) {
    pthread_t t1, t2;
    pthread_barrier_init(&barrier, NULL, 2);
    pthread_create(&t1, NULL, producer, NULL);
    pthread_create(&t2, NULL, consumer, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    return 0;
}

#else

static pthread_barrier_t b1, b2, b3;

static void *producer(void *arg) {
    (void)arg;
    int *data = malloc(sizeof(int));
    *data = 42;
    shared_ptr = data;

    pthread_barrier_wait(&b1);
    pthread_barrier_wait(&b2);

    free(data);
    shared_ptr = NULL;
    pthread_barrier_wait(&b3);
    return NULL;
}

static void *consumer(void *arg) {
    (void)arg;
    pthread_barrier_wait(&b1);

    int *local = shared_ptr;

    pthread_barrier_wait(&b2);
    pthread_barrier_wait(&b3);

    if (local)
        printf("val=%d\n", *local);
    return NULL;
}

int main(void) {
    pthread_t t1, t2;
    pthread_barrier_init(&b1, NULL, 2);
    pthread_barrier_init(&b2, NULL, 2);
    pthread_barrier_init(&b3, NULL, 2);
    pthread_create(&t1, NULL, producer, NULL);
    pthread_create(&t2, NULL, consumer, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    return 0;
}

#endif

#elif defined(CASE_UBSAN_CLEAN) || defined(CASE_UBSAN_UB)

#ifdef RSCHED
#include "rsched.h"
#endif
#include <limits.h>
#include <pthread.h>
#include <stdio.h>

#if defined(CASE_UBSAN_CLEAN)
static volatile long shared = INT_MAX;
#else
static volatile int shared = INT_MAX;
#endif

static void *worker(void *arg) {
    (void)arg;
#if defined(CASE_UBSAN_CLEAN)
    long y = shared + 1L;
    printf("y=%ld\n", y);
#else
    int y = shared + 1;
    printf("y=%d\n", y);
#endif
    return NULL;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    return 0;
}

#elif defined(CASE_TSAN_CLEAN) || defined(CASE_TSAN_RACE)

#include <pthread.h>
#include <stdio.h>

static int counter = 0;

#if defined(CASE_TSAN_CLEAN)
static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
#endif

static void *increment(void *arg) {
    (void)arg;
#if defined(CASE_TSAN_CLEAN)
    pthread_mutex_lock(&mtx);
#endif
    counter++;
#if defined(CASE_TSAN_CLEAN)
    pthread_mutex_unlock(&mtx);
#endif
    return NULL;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, increment, NULL);
#if defined(CASE_TSAN_CLEAN)
    pthread_mutex_lock(&mtx);
#endif
    counter++;
#if defined(CASE_TSAN_CLEAN)
    pthread_mutex_unlock(&mtx);
#endif
    pthread_join(t, NULL);
    printf("counter = %d\n", counter);
    return 0;
}

#else
#error "define one sanitizer CASE_* variant"
#endif
