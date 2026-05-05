/* uniform-lock.c – like uniform.c but with a mutex guarding part of the
 * critical section in each thread.
 *
 * run_uniform_lock(seed) returns the final value of x.
 */
#include "rsched.h"
#include <stdlib.h>

static int x;
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t bar;

static void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    sched_yield(); x = (x << 1);
    pthread_mutex_lock(&mutex);
    sched_yield(); x = (x << 1);
    sched_yield(); x = (x << 1);
    pthread_mutex_unlock(&mutex);
    sched_yield(); x = (x << 1);
    sched_yield(); x = (x << 1);
    sched_yield(); x = (x << 1);
    return NULL;
}

static void *thread2(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    sched_yield(); x = (x << 1) + 1;
    sched_yield(); x = (x << 1) + 1;
    sched_yield(); x = (x << 1) + 1;
    sched_yield(); x = (x << 1) + 1;
    pthread_mutex_lock(&mutex);
    sched_yield(); x = (x << 1) + 1;
    sched_yield(); x = (x << 1) + 1;
    pthread_mutex_unlock(&mutex);
    return NULL;
}

int run_uniform_lock(unsigned long long seed) {
    rsched_reinit(seed);
    x = 0;
    mutex = (pthread_mutex_t)PTHREAD_MUTEX_INITIALIZER;

    pthread_t threads[2];
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&threads[0], NULL, thread1, NULL);
    pthread_create(&threads[1], NULL, thread2, NULL);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    return x;
}
