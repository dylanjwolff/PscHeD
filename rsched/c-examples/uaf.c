/* uaf.c – use-after-free race detection test.
 *
 * The writer sets a node's magic field and then changes it.
 * The reader captures the pointer, yields to let the writer run, then reads
 * the magic field – producing a data race if the writer has run between the
 * two reader steps.
 *
 * run_uaf(seed) returns 1 if the race was observed, 0 otherwise.
 */
#include "rsched.h"
#include <stdlib.h>

struct Node {
    volatile int          value;
    volatile unsigned int magic;
};

static volatile struct Node *shared_node;
static pthread_barrier_t     barrier;
static int                   race_detected;

static void *writer(void *arg) {
    (void)arg;
    shared_node = (struct Node *)malloc(sizeof(struct Node));
    shared_node->value = 42;
    shared_node->magic = 0xDEADBEEFu;

    pthread_barrier_wait(&barrier);
    sched_yield(); /* give reader a chance to capture the pointer */

    shared_node->magic = 0xBADC0DEu;
    shared_node = NULL;
    return NULL;
}

static void *reader(void *arg) {
    (void)arg;
    pthread_barrier_wait(&barrier);

    /* Capture pointer, then yield so the writer can change magic. */
    struct Node *local = (struct Node *)shared_node;
    sched_yield();

    if (local != NULL && local->magic != 0xDEADBEEFu) {
        race_detected = 1;
    }
    return NULL;
}

int run_uaf(unsigned long long seed) {
    rsched_reinit(seed);
    shared_node   = NULL;
    race_detected = 0;

    pthread_t t1, t2;
    pthread_barrier_init(&barrier, NULL, 2);
    pthread_create(&t1, NULL, writer, NULL);
    pthread_create(&t2, NULL, reader, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    if (shared_node != NULL) {
        free((void *)shared_node);
        shared_node = NULL;
    }
    return race_detected;
}
