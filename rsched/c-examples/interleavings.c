#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <pthread.h>
#  include <sched.h>
#  include <stdatomic.h>
#  include <stdint.h>
#endif

#include <stdlib.h>

static unsigned long long lcg_next(unsigned long long s) {
    return s * 6364136223846793005ULL + 1442695040888963407ULL;
}

// uniform / uniform-lock

#define UNIFORM_OPS 6

static _Atomic int uniform_x;
static pthread_mutex_t uniform_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t uniform_barrier;
static int uniform_xor_consts[UNIFORM_OPS];
static int uniform_use_lock;

static void *uniform_add_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&uniform_barrier);

    if (!uniform_use_lock) {
        for (int i = 0; i < 5; i++)
            atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
        return NULL;
    }

    atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
    pthread_mutex_lock(&uniform_mutex);
    atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
    pthread_mutex_unlock(&uniform_mutex);
    atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&uniform_x, 1, memory_order_seq_cst);
    return NULL;
}

static void *uniform_xor_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&uniform_barrier);

    if (!uniform_use_lock) {
        for (int i = 0; i < 5; i++)
            atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[i], memory_order_seq_cst);
        return NULL;
    }

    atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[0], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[1], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[2], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[3], memory_order_seq_cst);
    pthread_mutex_lock(&uniform_mutex);
    atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[4], memory_order_seq_cst);
    atomic_fetch_xor_explicit(&uniform_x, uniform_xor_consts[5], memory_order_seq_cst);
    pthread_mutex_unlock(&uniform_mutex);
    return NULL;
}

static int run_uniform_variant(unsigned long long seed, int use_lock) {
    int ops = use_lock ? 6 : 5;
    unsigned long long s = seed;
    for (int i = 0; i < ops; i++) {
        s = lcg_next(s);
        uniform_xor_consts[i] = (int)(s >> 33);
    }

    rsched_reinit(seed);
    uniform_use_lock = use_lock;
    uniform_mutex = (pthread_mutex_t)PTHREAD_MUTEX_INITIALIZER;
    atomic_store_explicit(&uniform_x, 0, memory_order_relaxed);

    pthread_t threads[2];
    pthread_barrier_init(&uniform_barrier, NULL, 2);
    pthread_create(&threads[0], NULL, uniform_add_worker, NULL);
    pthread_create(&threads[1], NULL, uniform_xor_worker, NULL);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    return atomic_load_explicit(&uniform_x, memory_order_relaxed);
}

int run_uniform(unsigned long long seed) {
    return run_uniform_variant(seed, 0);
}

int run_uniform_lock(unsigned long long seed) {
    return run_uniform_variant(seed, 1);
}

// uaf race fixture

struct Node {
    _Atomic int value;
    _Atomic unsigned int magic;
};

static struct Node *_Atomic uaf_shared_node;
static pthread_barrier_t uaf_barrier;
static int uaf_race_detected;

static void *uaf_writer(void *arg) {
    (void)arg;
    struct Node *n = (struct Node *)malloc(sizeof(struct Node));
    n->value = 42;
    atomic_store_explicit(&n->magic, 0xDEADBEEFu, memory_order_relaxed);
    atomic_store_explicit(&uaf_shared_node, n, memory_order_relaxed);

    pthread_barrier_wait(&uaf_barrier);

    atomic_store_explicit(&n->magic, 0xBADC0DEu, memory_order_seq_cst);
    atomic_store_explicit(&uaf_shared_node, (struct Node *)NULL, memory_order_seq_cst);
    return NULL;
}

static void *uaf_reader(void *arg) {
    (void)arg;
    pthread_barrier_wait(&uaf_barrier);

    struct Node *local = atomic_load_explicit(&uaf_shared_node, memory_order_seq_cst);
    if (local != NULL &&
        atomic_load_explicit(&local->magic, memory_order_seq_cst) != 0xDEADBEEFu) {
        uaf_race_detected = 1;
    }
    return NULL;
}

int run_uaf(unsigned long long seed) {
    rsched_reinit(seed);
    atomic_store_explicit(&uaf_shared_node, (struct Node *)NULL, memory_order_relaxed);
    uaf_race_detected = 0;

    pthread_t t1, t2;
    pthread_barrier_init(&uaf_barrier, NULL, 2);
    pthread_create(&t1, NULL, uaf_writer, NULL);
    pthread_create(&t2, NULL, uaf_reader, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    return uaf_race_detected;
}

// DFS interleaving counter

#define DFS_COUNT_OPS 2

static _Atomic int dfs_next_slot;
static pthread_barrier_t dfs_barrier;
static int dfs_order[DFS_COUNT_OPS * 2];

static void *dfs_record_ops(void *arg) {
    int id = (int)(intptr_t)arg;
    pthread_barrier_wait(&dfs_barrier);
    for (int i = 0; i < DFS_COUNT_OPS; i++) {
        int slot = atomic_fetch_add_explicit(&dfs_next_slot, 1, memory_order_seq_cst);
        dfs_order[slot] = id;
    }
    return NULL;
}

int run_dfs_count(void) {
    rsched_reinit(0);
    atomic_store_explicit(&dfs_next_slot, 0, memory_order_relaxed);
    for (int i = 0; i < DFS_COUNT_OPS * 2; i++)
        dfs_order[i] = 0;

    pthread_t threads[2];
    pthread_barrier_init(&dfs_barrier, NULL, 2);
    pthread_create(&threads[0], NULL, dfs_record_ops, (void *)(intptr_t)1);
    pthread_create(&threads[1], NULL, dfs_record_ops, (void *)(intptr_t)2);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    int encoded = 0;
    for (int i = 0; i < DFS_COUNT_OPS * 2; i++)
        encoded = encoded * 10 + dfs_order[i];
    return encoded;
}
