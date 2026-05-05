/* uniform.c – entrypoint for the "uniform" interleaving test.
 *
 * Two threads race to bit-shift a shared integer, yielding between each
 * operation.  Thread 1 shifts left (×2); Thread 2 shifts left and adds 1.
 * A barrier synchronises the start so all interleavings begin from x == 0.
 *
 * run_uniform(seed) returns the final value of x.
 */
#include "rsched.h"
#include <stdlib.h>

static int x;
static pthread_barrier_t bar;

static void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    sched_yield(); x = (x << 1);
    sched_yield(); x = (x << 1);
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
    sched_yield(); x = (x << 1) + 1;
    return NULL;
}

int run_uniform(unsigned long long seed) {
    rsched_reinit(seed);
    x = 0;

    pthread_t threads[2];
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&threads[0], NULL, thread1, NULL);
    pthread_create(&threads[1], NULL, thread2, NULL);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    return x;
}
