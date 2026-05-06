/*
 * tsan_no_race.c – two threads increment a shared counter protected by a
 * mutex.  TSAN must report no errors.
 *
 * pthread_mutex_lock/unlock are resolved to rsched's virtual scheduler via
 * librsched_preload.so.  rsched calls __tsan_acquire/__tsan_release (enabled
 * by --features tsan) so TSAN sees the correct happens-before edge through
 * each lock/unlock pair even though the underlying mutex is virtual.
 *
 * Build:  make tsan        (from c-examples/)
 * Run:    ./tsan_no_race   → exits 0, no TSAN warnings
 */
#include <pthread.h>
#include <stdio.h>

static int counter = 0;
static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;

static void *increment(void *arg)
{
    (void)arg;
    pthread_mutex_lock(&mtx);
    counter++;
    pthread_mutex_unlock(&mtx);
    return NULL;
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, increment, NULL);
    pthread_mutex_lock(&mtx);
    counter++;
    pthread_mutex_unlock(&mtx);
    pthread_join(t, NULL);
    printf("counter = %d\n", counter);
    return 0;
}
