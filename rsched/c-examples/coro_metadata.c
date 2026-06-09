#ifdef RSCHED
#  include "rsched.h"
#else
#  include <pthread.h>
#  include <sched.h>
#endif

static __thread int tls_value = -1;
static pthread_barrier_t bar;
static pthread_t expected[2];
static int failures[2];

static void *metadata_worker(void *arg) {
    int idx = (int)(long)arg;
    int value = 1000 + idx;

    pthread_barrier_wait(&bar);
    tls_value = value;

    if (!pthread_equal(pthread_self(), expected[idx])) {
        failures[idx] |= 1;
    }

    sched_yield();
    if (tls_value != value) {
        failures[idx] |= 2;
    }

    tls_value += 7;
    sched_yield();
    if (tls_value != value + 7) {
        failures[idx] |= 4;
    }

    if (!pthread_equal(pthread_self(), expected[idx])) {
        failures[idx] |= 8;
    }

    return (void *)(long)tls_value;
}

int run_coro_metadata(unsigned long long seed) {
    rsched_reinit(seed);

    failures[0] = 0;
    failures[1] = 0;
    tls_value = 42;

    pthread_t threads[2];
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&threads[0], NULL, metadata_worker, (void *)0);
    pthread_create(&threads[1], NULL, metadata_worker, (void *)1);
    expected[0] = threads[0];
    expected[1] = threads[1];

    void *ret0 = 0;
    void *ret1 = 0;
    pthread_join(threads[0], &ret0);
    pthread_join(threads[1], &ret1);

    int result = failures[0] | (failures[1] << 8);
    if ((long)ret0 != 1007) {
        result |= 1 << 16;
    }
    if ((long)ret1 != 1008) {
        result |= 1 << 17;
    }
    if (tls_value != 42) {
        result |= 1 << 18;
    }
    return result;
}
