/* uniform-lock.c – like uniform.c but with a mutex guarding part of the
 * critical section in each thread.
 *
 * Thread 1 does fetch_add(+1); thread 2 does fetch_xor with per-step
 * pseudo-random constants.  A mutex covers a subset of each thread's
 * operations, constraining some interleavings.
 *
 * run_uniform_lock(seed) returns the final value of x.
 */
#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <stdatomic.h>
#  include <pthread.h>
#  include <sched.h>
#endif
#include <stdlib.h>

#define N_OPS 6

static _Atomic int x;
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t bar;
static int xor_consts[N_OPS];

static unsigned long long lcg_next(unsigned long long s) {
    return s * 6364136223846793005ULL + 1442695040888963407ULL;
}

static void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    pthread_mutex_lock(&mutex);
    atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    pthread_mutex_unlock(&mutex);
    atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&x, 1, memory_order_seq_cst);
    return NULL;
}

static void *thread2(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    atomic_fetch_xor_explicit(&x, xor_consts[0], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&x, xor_consts[1], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&x, xor_consts[2], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&x, xor_consts[3], memory_order_seq_cst);
    pthread_mutex_lock(&mutex);
    atomic_fetch_xor_explicit(&x, xor_consts[4], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&x, xor_consts[5], memory_order_seq_cst);
    pthread_mutex_unlock(&mutex);
    return NULL;
}

int run_uniform_lock(unsigned long long seed) {
    unsigned long long s = seed;
    for (int i = 0; i < N_OPS; i++) {
        s = lcg_next(s);
        xor_consts[i] = (int)(s >> 33);
    }

    rsched_reinit(seed);
    atomic_store_explicit(&x, 0, memory_order_relaxed);
    mutex = (pthread_mutex_t)PTHREAD_MUTEX_INITIALIZER;

    pthread_t threads[2];
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&threads[0], NULL, thread1, NULL);
    pthread_create(&threads[1], NULL, thread2, NULL);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    return atomic_load_explicit(&x, memory_order_relaxed);
}
