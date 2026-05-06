/*
 * tsan_race.c – deliberate data race: two threads increment a shared counter
 * without any synchronisation.  TSAN must report a data race.
 *
 * Linked dynamically against librsched_preload.so (built with --features tsan)
 * so that pthread_create/join resolve to rsched's scheduler.  The race is in
 * unsynchronised counter++ accesses, which TSAN detects directly.
 *
 * Build:  make tsan    (from c-examples/)
 * Run:    ./tsan_race  → TSAN prints "WARNING: ThreadSanitizer: data race"
 */
#include <pthread.h>
#include <stdio.h>

static int counter = 0;

static void *increment(void *arg)
{
    (void)arg;
    counter++;   /* data race: unsynchronised write concurrent with main */
    return NULL;
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, increment, NULL);
    counter++;   /* data race: unsynchronised write concurrent with thread */
    pthread_join(t, NULL);
    printf("counter = %d\n", counter);
    return 0;
}
