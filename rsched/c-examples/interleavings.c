#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <pthread.h>
#  include <sched.h>
#  include <stdatomic.h>
#  include <stdint.h>
#endif

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

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

// Small bounded variants used by DFS tests. The full uniform fixtures above
// have enough scheduling points to exceed the fixed task table during one
// exhaustive in-process DFS run.

static _Atomic int uniform_dfs_x;
static pthread_barrier_t uniform_dfs_barrier;
static pthread_mutex_t uniform_dfs_mutex = PTHREAD_MUTEX_INITIALIZER;
static int uniform_dfs_use_lock;

static void *uniform_dfs_add_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&uniform_dfs_barrier);

    if (uniform_dfs_use_lock) {
        pthread_mutex_lock(&uniform_dfs_mutex);
        atomic_fetch_add_explicit(&uniform_dfs_x, 1, memory_order_seq_cst);
        pthread_mutex_unlock(&uniform_dfs_mutex);
        return NULL;
    }
    atomic_fetch_add_explicit(&uniform_dfs_x, 1, memory_order_seq_cst);
    atomic_fetch_add_explicit(&uniform_dfs_x, 1, memory_order_seq_cst);
    return NULL;
}

static void *uniform_dfs_xor_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&uniform_dfs_barrier);

    if (uniform_dfs_use_lock) {
        pthread_mutex_lock(&uniform_dfs_mutex);
        atomic_fetch_xor_explicit(&uniform_dfs_x, 3, memory_order_seq_cst);
        pthread_mutex_unlock(&uniform_dfs_mutex);
        return NULL;
    }
    atomic_fetch_xor_explicit(&uniform_dfs_x, 3, memory_order_seq_cst);
    atomic_fetch_xor_explicit(&uniform_dfs_x, 5, memory_order_seq_cst);
    return NULL;
}

static int run_uniform_dfs_variant(int use_lock) {
    rsched_reinit(0);
    uniform_dfs_use_lock = use_lock;
    uniform_dfs_mutex = (pthread_mutex_t)PTHREAD_MUTEX_INITIALIZER;
    atomic_store_explicit(&uniform_dfs_x, 0, memory_order_relaxed);

    pthread_t threads[2];
    pthread_barrier_init(&uniform_dfs_barrier, NULL, 2);
    pthread_create(&threads[0], NULL, uniform_dfs_add_worker, NULL);
    pthread_create(&threads[1], NULL, uniform_dfs_xor_worker, NULL);
    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    return atomic_load_explicit(&uniform_dfs_x, memory_order_relaxed);
}

int run_uniform_dfs(void) {
    return run_uniform_dfs_variant(0);
}

int run_uniform_lock_dfs(void) {
    return run_uniform_dfs_variant(1);
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

// Standalone variants used by tests that need a separately linked binary.

#if defined(STANDALONE_COUNTER)

#define COUNTER_OPS 10

static _Atomic int standalone_counter;
static pthread_barrier_t standalone_counter_barrier;

static void *standalone_counter_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&standalone_counter_barrier);
    for (int i = 0; i < COUNTER_OPS; i++)
        atomic_fetch_add_explicit(&standalone_counter, 1, memory_order_seq_cst);
    return NULL;
}

int main(void) {
    atomic_store_explicit(&standalone_counter, 0, memory_order_relaxed);

    pthread_t t1, t2;
    pthread_barrier_init(&standalone_counter_barrier, NULL, 2);
    pthread_create(&t1, NULL, standalone_counter_worker, NULL);
    pthread_create(&t2, NULL, standalone_counter_worker, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    int result = atomic_load_explicit(&standalone_counter, memory_order_relaxed);
    if (result != 2 * COUNTER_OPS) {
        fprintf(stderr, "expected counter=%d, got %d\n", 2 * COUNTER_OPS, result);
        return 1;
    }
    return 0;
}

#elif defined(STANDALONE_DFS_COUNT)

#if defined(TASK_BACKEND_FORK)

#define PROCESS_DFS_OPS 1

struct process_dfs_trace {
    _Atomic int len;
    char trace[PROCESS_DFS_OPS * 2 + 1];
};

static void process_record_ops(struct process_dfs_trace *shared, char tag) {
    for (int i = 0; i < PROCESS_DFS_OPS; i++) {
        int pos = atomic_fetch_add_explicit(&shared->len, 1, memory_order_seq_cst);
        if (pos >= 0 && pos < PROCESS_DFS_OPS * 2)
            shared->trace[pos] = tag;
    }
}

static int run_process_dfs_once(char out[PROCESS_DFS_OPS * 2 + 1]) {
    rsched_reinit(0);

    struct process_dfs_trace *shared = mmap(0, sizeof(*shared), PROT_READ | PROT_WRITE,
                                            MAP_ANONYMOUS | MAP_SHARED, -1, 0);
    if (shared == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    atomic_store_explicit(&shared->len, 0, memory_order_relaxed);
    for (int i = 0; i < PROCESS_DFS_OPS * 2 + 1; i++)
        shared->trace[i] = 0;

    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 2;
    }

    if (child == 0) {
        process_record_ops(shared, 'C');
#ifdef RSCHED
        rsched_process_exit();
#endif
        _exit(0);
    }

    process_record_ops(shared, 'P');

    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) {
            perror("waitpid");
            return 2;
        }
    }

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child failed: status=%d\n", status);
        return 1;
    }
    int len = atomic_load_explicit(&shared->len, memory_order_relaxed);
    if (len != PROCESS_DFS_OPS * 2) {
        fprintf(stderr, "bad trace length: %d\n", len);
        return 1;
    }

    for (int i = 0; i < PROCESS_DFS_OPS * 2; i++)
        out[i] = shared->trace[i];
    out[PROCESS_DFS_OPS * 2] = 0;
    return 0;
}

static int process_trace_bit(const char *trace) {
    if (!strcmp(trace, "PC"))
        return 1 << 0;
    if (!strcmp(trace, "CP"))
        return 1 << 1;
    return 0;
}

int main(void) {
    setenv("RSCHED_SCHEDULER", "dfs", 1);
    rsched_dfs_reset();

    int mask = 0;
    size_t runs = 0;
    while (rsched_dfs_has_next()) {
        char trace[PROCESS_DFS_OPS * 2 + 1];
        int r = run_process_dfs_once(trace);
        if (r != 0)
            return r;
        int bit = process_trace_bit(trace);
        if (bit == 0) {
            fprintf(stderr, "unexpected trace: %s\n", trace);
            return 1;
        }
        mask |= bit;
        rsched_dfs_finish_current();
        runs++;
        if (runs > 1024) {
            fprintf(stderr, "too many dfs runs\n");
            return 1;
        }
    }

    printf("runs=%zu completed=%zu mask=0x%x\n", runs,
           rsched_dfs_completed_schedules(), mask);
    return mask == 0x3 && runs == rsched_dfs_completed_schedules() ? 0 : 1;
}

#else

int main(void) {
    setenv("RSCHED_SCHEDULER", "dfs", 1);
    rsched_dfs_reset();

    int mask = 0;
    size_t runs = 0;
    while (rsched_dfs_has_next()) {
        switch (run_dfs_count()) {
        case 1122:
            mask |= 1 << 0;
            break;
        case 1212:
            mask |= 1 << 1;
            break;
        case 1221:
            mask |= 1 << 2;
            break;
        case 2112:
            mask |= 1 << 3;
            break;
        case 2121:
            mask |= 1 << 4;
            break;
        case 2211:
            mask |= 1 << 5;
            break;
        default:
            fprintf(stderr, "unexpected encoded trace\n");
            return 1;
        }
        rsched_dfs_finish_current();
        runs++;
    }

    printf("runs=%zu completed=%zu mask=0x%x\n", runs,
           rsched_dfs_completed_schedules(), mask);
    return mask == 0x3f && runs == rsched_dfs_completed_schedules() ? 0 : 1;
}

#endif
#endif
