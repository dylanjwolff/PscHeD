/* uniform-lock.c – like uniform.c but with a mutex guarding part of the
 * critical section in each thread.
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

static _Atomic int x;
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t bar;

static void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    pthread_mutex_lock(&mutex);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    pthread_mutex_unlock(&mutex);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    return NULL;
}

static void *thread2(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    pthread_mutex_lock(&mutex);
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    pthread_mutex_unlock(&mutex);
    return NULL;
}

int run_uniform_lock(unsigned long long seed) {
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
