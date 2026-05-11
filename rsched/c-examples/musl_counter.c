/* musl_counter.c – atomic counter test for musl libc compatibility.
 *
 * Two threads each increment a shared atomic counter N times.
 * Returns 0 if the final count equals 2*N, 1 otherwise.
 *
 * Covered by: cargo test -p rsched --test musl
 */
#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <stdatomic.h>
#  include <pthread.h>
#  include <sched.h>
#endif
#include <stdio.h>

#define N_OPS 10

static _Atomic int counter;
static pthread_barrier_t bar;

static void *incrementer(void *arg)
{
    (void)arg;
    pthread_barrier_wait(&bar);
    for (int i = 0; i < N_OPS; i++)
        atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
    return NULL;
}

int main(void)
{
    atomic_store_explicit(&counter, 0, memory_order_relaxed);

    pthread_t t1, t2;
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&t1, NULL, incrementer, NULL);
    pthread_create(&t2, NULL, incrementer, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    int result = atomic_load_explicit(&counter, memory_order_relaxed);
    if (result != 2 * N_OPS) {
        fprintf(stderr, "expected counter=%d, got %d\n", 2 * N_OPS, result);
        return 1;
    }
    return 0;
}
