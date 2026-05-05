/* uniform.c – interleaving test: two threads race to bit-shift a shared int.
 *
 * Thread 1 shifts left (×2); Thread 2 shifts left and sets the low bit.
 * A barrier synchronises the start so all interleavings begin from x == 0.
 *
 * Each atomic load and store is a scheduling point when built with rsched.
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

static _Atomic int x;
static pthread_barrier_t bar;

static void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
    atomic_store_explicit(&x, atomic_load_explicit(&x, memory_order_seq_cst) << 1, memory_order_seq_cst);
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
    atomic_store_explicit(&x, (atomic_load_explicit(&x, memory_order_seq_cst) << 1) | 1, memory_order_seq_cst);
    return NULL;
}

int run_uniform(unsigned long long seed) {
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
