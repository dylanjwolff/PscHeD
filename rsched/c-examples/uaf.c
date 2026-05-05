/* uaf.c – use-after-free race detection test.
 *
 * The writer allocates a node, initialises its magic field, then (after a
 * barrier) overwrites magic and nulls the shared pointer.  The reader captures
 * the pointer with an atomic load, then reads magic with another atomic load.
 * Each atomic load/store is a scheduling point (when built with rsched), so
 * the scheduler can interpose between capture and read, letting the writer
 * corrupt magic in the window between them.
 *
 * run_uaf(seed) returns 1 if the race was observed, 0 otherwise.
 */
#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <stdatomic.h>
#  include <pthread.h>
#  include <sched.h>
#endif
#include <stdlib.h>

struct Node {
    _Atomic int          value;
    _Atomic unsigned int magic;
};

static struct Node * _Atomic shared_node;
static pthread_barrier_t     barrier;
static int                   race_detected;

static void *writer(void *arg) {
    (void)arg;
    struct Node *n = (struct Node *)malloc(sizeof(struct Node));
    n->value = 42;
    atomic_store_explicit(&n->magic, 0xDEADBEEFu, memory_order_relaxed);
    /* No scheduling point needed before the barrier – reader hasn't started. */
    atomic_store_explicit(&shared_node, n, memory_order_relaxed);

    pthread_barrier_wait(&barrier);

    /* Scheduling point before the store: the reader may capture shared_node
     * (still non-NULL) between the yield and this write, then see the bad
     * magic value when it loads below. */
    atomic_store_explicit(&n->magic, 0xBADC0DEu, memory_order_seq_cst);
    atomic_store_explicit(&shared_node, (struct Node *)NULL, memory_order_seq_cst);
    return NULL;
}

static void *reader(void *arg) {
    (void)arg;
    pthread_barrier_wait(&barrier);

    /* Scheduling point before the load: writer may change magic between the
     * yield and this load, but the pointer is captured here. */
    struct Node *local = atomic_load_explicit(&shared_node, memory_order_seq_cst);

    /* Another scheduling point: writer may overwrite magic before this load. */
    if (local != NULL &&
            atomic_load_explicit(&local->magic, memory_order_seq_cst) != 0xDEADBEEFu) {
        race_detected = 1;
    }
    return NULL;
}

int run_uaf(unsigned long long seed) {
    rsched_reinit(seed);
    atomic_store_explicit(&shared_node, (struct Node *)NULL, memory_order_relaxed);
    race_detected = 0;

    pthread_t t1, t2;
    pthread_barrier_init(&barrier, NULL, 2);
    pthread_create(&t1, NULL, writer, NULL);
    pthread_create(&t2, NULL, reader, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    return race_detected;
}
