/* uniform.c – interleaving test: two threads race on a shared int.
 *
 * Thread 1 increments x with atomic_fetch_add (+1 each step).
 * Thread 2 flips bits with atomic_fetch_xor using a different pseudo-random
 * constant each step, derived from the seed before the scheduler starts.
 *
 * Each atomic RMW is a single scheduling point (yield + operation) when built
 * with rsched, giving the scheduler one interleaving decision per operation.
 *
 * run_uniform(seed) returns the final value of x.
 */
#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <stdatomic.h>
#  include <pthread.h>
#  include <sched.h>
#endif

#define N_OPS 5

static _Atomic int x;
static pthread_barrier_t bar;
static int xor_consts[N_OPS];

/* Simple LCG to generate xor constants from the seed before rsched starts. */
static unsigned long long lcg_next(unsigned long long s) {
    return s * 6364136223846793005ULL + 1442695040888963407ULL;
}

static void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    for (int i = 0; i < N_OPS; i++)
        atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    return NULL;
}

static void *thread2(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    for (int i = 0; i < N_OPS; i++)
        atomic_fetch_xor_explicit(&x, xor_consts[i], memory_order_seq_cst);
    return NULL;
}

int run_uniform(unsigned long long seed) {
    unsigned long long s = seed;
    for (int i = 0; i < N_OPS; i++) {
        s = lcg_next(s);
        xor_consts[i] = (int)(s >> 33);
    }

    rsched_reinit(seed);
    atomic_store_explicit(&x, 0, memory_order_relaxed);

    pthread_t threads[2];
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&threads[0], NULL, thread1, NULL);
    pthread_create(&threads[1], NULL, thread2, NULL);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    return atomic_load_explicit(&x, memory_order_relaxed);
}
