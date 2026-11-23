/* Semaphores, timed waits, rwlocks, pthread_once, and sleeps under DFS.
 *
 * Every schedule must finish with the right result: an unmodeled blocking
 * call would hang the only running task, and a wrong model would break the
 * checked invariant. Build one case with -DCASE_SYNC_<NAME>. Without -DRSCHED
 * the case runs once on the native scheduler.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#ifdef RSCHED
#include "rsched.h"
#endif

#define MAX_RUNS 4096

static int fail(const char *what) {
    fprintf(stderr, "FAIL: %s\n", what);
    return 1;
}

__attribute__((unused)) static double now(clockid_t clock) {
    struct timespec ts;
    clock_gettime(clock, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

__attribute__((unused)) static struct timespec in_ms(clockid_t clock, long ms) {
    struct timespec ts;
    clock_gettime(clock, &ts);
    ts.tv_nsec += ms * 1000000L;
    ts.tv_sec += ts.tv_nsec / 1000000000L;
    ts.tv_nsec %= 1000000000L;
    return ts;
}

#if defined(CASE_SYNC_SEM)

/* A consumer blocks on an empty semaphore until the producer posts. */
static sem_t sem;
static int produced;

static void *producer(void *arg) {
    (void)arg;
    produced = 42;
    sem_post(&sem);
    return 0;
}

static int run_once(void) {
    sem_init(&sem, 0, 0);
    produced = 0;
    pthread_t t;
    pthread_create(&t, 0, producer, 0);
    if (sem_wait(&sem) != 0)
        return fail("sem_wait");
    int seen = produced;
    pthread_join(t, 0);
    if (sem_trywait(&sem) != -1 || errno != EAGAIN)
        return fail("sem_trywait on empty semaphore");
    sem_destroy(&sem);
    return seen == 42 ? 0 : fail("consumer ran before the post");
}

#elif defined(CASE_SYNC_SEM_TIMEDWAIT)

/* Nobody posts, so the wait expires, but never before its deadline. */
static int run_once(void) {
    sem_t sem;
    sem_init(&sem, 0, 0);
    double start = now(CLOCK_REALTIME);
    struct timespec deadline = in_ms(CLOCK_REALTIME, 20);
    if (sem_timedwait(&sem, &deadline) != -1 || errno != ETIMEDOUT)
        return fail("sem_timedwait did not time out");
    if (now(CLOCK_REALTIME) - start < 0.019)
        return fail("sem_timedwait timed out early");
    sem_destroy(&sem);
    return 0;
}

#elif defined(CASE_SYNC_COND_TIMEDWAIT)

/* A signal may arrive before the deadline or not at all; either way the
   waiter returns holding the mutex, with ETIMEDOUT only after the deadline. */
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;

static void *signaler(void *arg) {
    (void)arg;
    pthread_mutex_lock(&mutex);
    ready = 1;
    pthread_cond_signal(&cond);
    pthread_mutex_unlock(&mutex);
    return 0;
}

static int run_once(void) {
    ready = 0;
    pthread_t t;
    pthread_create(&t, 0, signaler, 0);
    pthread_mutex_lock(&mutex);
    double start = now(CLOCK_REALTIME);
    struct timespec deadline = in_ms(CLOCK_REALTIME, 20);
    int r = 0;
    while (!ready && r == 0)
        r = pthread_cond_timedwait(&cond, &mutex, &deadline);
    if (r == ETIMEDOUT && now(CLOCK_REALTIME) - start < 0.019)
        return fail("pthread_cond_timedwait timed out early");
    if (r != 0 && r != ETIMEDOUT)
        return fail("pthread_cond_timedwait failed");
    pthread_mutex_unlock(&mutex);
    pthread_join(t, 0);
    return 0;
}

#elif defined(CASE_SYNC_RWLOCK)

/* Readers may share the lock; the writer excludes everyone. */
static pthread_rwlock_t rwlock = PTHREAD_RWLOCK_INITIALIZER;
static int readers, writers;

static int check(void) { return writers > 1 || (writers == 1 && readers != 0); }

static void *reader(void *arg) {
    (void)arg;
    pthread_rwlock_rdlock(&rwlock);
    readers++;
    sched_yield();
    long bad = check();
    readers--;
    pthread_rwlock_unlock(&rwlock);
    return (void *)bad;
}

static int run_once(void) {
    readers = writers = 0;
    pthread_t t;
    pthread_create(&t, 0, reader, 0);
    pthread_rwlock_wrlock(&rwlock);
    writers++;
    sched_yield();
    int bad = check();
    writers--;
    pthread_rwlock_unlock(&rwlock);
    void *reader_bad;
    pthread_join(t, &reader_bad);
    if (bad || reader_bad)
        return fail("rwlock exclusion violated");
    if (pthread_rwlock_tryrdlock(&rwlock) != 0 || pthread_rwlock_trywrlock(&rwlock) != EBUSY)
        return fail("rwlock try operations");
    pthread_rwlock_unlock(&rwlock);
    return 0;
}

#elif defined(CASE_SYNC_ONCE)

/* Two threads race to pthread_once; init runs exactly once and both see it
   finished. */
static pthread_once_t once;
static int inits;

static void init(void) {
    sched_yield();
    inits++;
}

static void *caller(void *arg) {
    (void)arg;
    pthread_once(&once, init);
    return (void *)(long)(inits != 1);
}

static int run_once(void) {
    once = (pthread_once_t)PTHREAD_ONCE_INIT;
    inits = 0;
    pthread_t t;
    pthread_create(&t, 0, caller, 0);
    pthread_once(&once, init);
    int bad = inits != 1;
    void *other_bad;
    pthread_join(t, &other_bad);
    return bad || other_bad ? fail("pthread_once") : 0;
}

#elif defined(CASE_SYNC_SLEEP)

/* A sleeping task lets the other run, and wakes after its duration. */
static int flag;

static void *setter(void *arg) {
    (void)arg;
    flag = 1;
    return 0;
}

static int run_once(void) {
    flag = 0;
    pthread_t t;
    pthread_create(&t, 0, setter, 0);
    double start = now(CLOCK_MONOTONIC);
    usleep(10 * 1000);
    if (now(CLOCK_MONOTONIC) - start < 0.0095)
        return fail("usleep returned early");
    struct timespec ts = {0, 1000000};
    nanosleep(&ts, 0);
    pthread_join(t, 0);
    return flag == 1 ? 0 : fail("setter did not run");
}

#elif defined(CASE_SYNC_DEEP_DFS)

/* Two threads contend for a mutex far more often than DFS once recorded
   (256 branching points). Only the first schedule runs. */
#define DFS_RUNS 1
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static int counter;

static void *bump(void *arg) {
    (void)arg;
    for (int i = 0; i < 500; i++) {
        pthread_mutex_lock(&mutex);
        counter++;
        pthread_mutex_unlock(&mutex);
    }
    return 0;
}

static int run_once(void) {
    counter = 0;
    pthread_t t;
    pthread_create(&t, 0, bump, 0);
    bump(0);
    pthread_join(t, 0);
    return counter == 1000 ? 0 : fail("lost an increment");
}

#else
#error "define one CASE_SYNC_* variant"
#endif

int main(void) {
#ifdef RSCHED
    setenv("RSCHED_SCHEDULER", "dfs", 1);
    rsched_init();
    rsched_dfs_reset();
    size_t runs = 0;
    while (rsched_dfs_has_next()) {
        rsched_reinit(0);
        int r = run_once();
        if (r != 0) {
            fprintf(stderr, "schedule %zu failed\n", runs);
            return r;
        }
        rsched_dfs_finish_current();
        if (++runs >= MAX_RUNS)
            return fail("too many dfs runs");
#ifdef DFS_RUNS
        if (runs == DFS_RUNS)
            break;
#endif
    }
    printf("runs=%zu completed=%zu\n", runs, rsched_dfs_completed_schedules());
    return runs == rsched_dfs_completed_schedules() ? 0 : 1;
#else
    int r = run_once();
    printf("runs=1\n");
    return r;
#endif
}
