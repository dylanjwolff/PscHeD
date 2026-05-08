/*
 * C kernels for Criterion benchmarks.
 *
 * These mirror Shuttle's create, counter, lock, and bounded-buffer benchmarks,
 * but run through rsched's C pthread/atomic interception layer.
 */
#ifdef RSCHED
#  include "rsched_atomic.h"
#else
#  include <stdatomic.h>
#  include <pthread.h>
#  include <sched.h>
#endif

#include <assert.h>
#include <stdint.h>
#include <stdlib.h>

static _Atomic int atomic_counter;
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t start_barrier;
static unsigned int locked_counter;

static unsigned int num_tasks;
static unsigned int events_per_task;

static void *create_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&start_barrier);
    atomic_fetch_add_explicit(&atomic_counter, 1, memory_order_seq_cst);
    return NULL;
}

static void *counter_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&start_barrier);
    for (unsigned int i = 0; i < events_per_task; i++) {
        atomic_fetch_add_explicit(&atomic_counter, 1, memory_order_seq_cst);
    }
    return NULL;
}

static void *lock_worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&start_barrier);
    for (unsigned int i = 0; i < events_per_task; i++) {
        pthread_mutex_lock(&mutex);
        locked_counter++;
        pthread_mutex_unlock(&mutex);
    }
    return NULL;
}

static void run_threads(unsigned long long seed, void *(*worker)(void *)) {
    rsched_reinit(seed);

    pthread_t *threads = (pthread_t *)calloc(num_tasks, sizeof(pthread_t));
    assert(threads != NULL);
    pthread_barrier_init(&start_barrier, NULL, num_tasks + 1);

    for (unsigned int i = 0; i < num_tasks; i++) {
        pthread_create(&threads[i], NULL, worker, NULL);
    }
    pthread_barrier_wait(&start_barrier);
    for (unsigned int i = 0; i < num_tasks; i++) {
        pthread_join(threads[i], NULL);
    }

    free(threads);
}

unsigned int run_bench_create(unsigned long long seed, unsigned int tasks) {
    num_tasks = tasks;
    atomic_store_explicit(&atomic_counter, 0, memory_order_relaxed);

    run_threads(seed, create_worker);

    return (unsigned int)atomic_load_explicit(&atomic_counter, memory_order_relaxed);
}

unsigned int run_bench_counter(
        unsigned long long seed,
        unsigned int tasks,
        unsigned int events) {
    num_tasks = tasks;
    events_per_task = events;
    atomic_store_explicit(&atomic_counter, 0, memory_order_relaxed);

    run_threads(seed, counter_worker);

    return (unsigned int)atomic_load_explicit(&atomic_counter, memory_order_relaxed);
}

unsigned int run_bench_lock(
        unsigned long long seed,
        unsigned int tasks,
        unsigned int events) {
    num_tasks = tasks;
    events_per_task = events;
    locked_counter = 0;
    mutex = (pthread_mutex_t)PTHREAD_MUTEX_INITIALIZER;

    run_threads(seed, lock_worker);

    return locked_counter;
}

static pthread_mutex_t buffer_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t has_space = PTHREAD_COND_INITIALIZER;
static pthread_cond_t has_elements = PTHREAD_COND_INITIALIZER;
static unsigned int buffer_count;
static unsigned int max_queue_size;
static unsigned int producer_events;
static unsigned int consumer_events;

static void *producer_worker(void *arg) {
    (void)arg;
    for (unsigned int i = 0; i < producer_events; i++) {
        pthread_mutex_lock(&buffer_mutex);
        while (buffer_count == max_queue_size) {
            pthread_cond_wait(&has_space, &buffer_mutex);
        }
        buffer_count++;
        pthread_cond_signal(&has_elements);
        pthread_mutex_unlock(&buffer_mutex);
    }
    return NULL;
}

static void *consumer_worker(void *arg) {
    (void)arg;
    for (unsigned int i = 0; i < consumer_events; i++) {
        pthread_mutex_lock(&buffer_mutex);
        while (buffer_count == 0) {
            pthread_cond_wait(&has_elements, &buffer_mutex);
        }
        buffer_count--;
        pthread_cond_signal(&has_space);
        pthread_mutex_unlock(&buffer_mutex);
    }
    return NULL;
}

unsigned int run_bench_buffer(
        unsigned long long seed,
        unsigned int producers,
        unsigned int consumers,
        unsigned int total_events,
        unsigned int queue_size) {
    assert(producers != 0);
    assert(consumers != 0);
    assert(total_events % producers == 0);
    assert(total_events % consumers == 0);

    rsched_reinit(seed);
    buffer_mutex = (pthread_mutex_t)PTHREAD_MUTEX_INITIALIZER;
    has_space = (pthread_cond_t)PTHREAD_COND_INITIALIZER;
    has_elements = (pthread_cond_t)PTHREAD_COND_INITIALIZER;
    buffer_count = 0;
    max_queue_size = queue_size;
    producer_events = total_events / producers;
    consumer_events = total_events / consumers;

    unsigned int total_threads = producers + consumers;
    pthread_t *threads = (pthread_t *)calloc(total_threads, sizeof(pthread_t));
    assert(threads != NULL);

    for (unsigned int i = 0; i < consumers; i++) {
        pthread_create(&threads[i], NULL, consumer_worker, NULL);
    }
    for (unsigned int i = 0; i < producers; i++) {
        pthread_create(&threads[consumers + i], NULL, producer_worker, NULL);
    }
    for (unsigned int i = 0; i < total_threads; i++) {
        pthread_join(threads[i], NULL);
    }

    free(threads);
    return buffer_count;
}
