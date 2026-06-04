#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <pthread.h>
#  include <sched.h>
#  include <stdatomic.h>
#  include <stdint.h>
#endif

#define N_OPS 2

static _Atomic int next_slot;
static pthread_barrier_t bar;
static int order[N_OPS * 2];

static void *record_ops(void *arg) {
    int id = (int)(intptr_t)arg;
    pthread_barrier_wait(&bar);
    for (int i = 0; i < N_OPS; i++) {
        int slot = atomic_fetch_add_explicit(&next_slot, 1, memory_order_seq_cst);
        order[slot] = id;
    }
    return NULL;
}

int run_dfs_count(void) {
    rsched_reinit(0);
    atomic_store_explicit(&next_slot, 0, memory_order_relaxed);
    for (int i = 0; i < N_OPS * 2; i++)
        order[i] = 0;

    pthread_t threads[2];
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&threads[0], NULL, record_ops, (void *)(intptr_t)1);
    pthread_create(&threads[1], NULL, record_ops, (void *)(intptr_t)2);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    int encoded = 0;
    for (int i = 0; i < N_OPS * 2; i++)
        encoded = encoded * 10 + order[i];
    return encoded;
}
