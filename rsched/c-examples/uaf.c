/* Port of zigsched/toy-examples/uaf.c
 *
 * A writer allocates a node and then frees it.  A reader may observe the
 * pointer as non-null after the writer has freed it, triggering a
 * use-after-free.  The assertion fires when this race is hit.
 *
 * Compile: see c-examples/Makefile
 */
#include "rsched.h"
#include <stdio.h>
#include <stdlib.h>
#include <assert.h>

struct Node {
    volatile int  value;
    volatile unsigned int magic;
};

volatile struct Node *shared_node = NULL;
pthread_barrier_t barrier;

void *writer(void *arg) {
    (void)arg;
    shared_node = (struct Node *)malloc(sizeof(struct Node));
    shared_node->value = 42;
    shared_node->magic = 0xDEADBEEF;

    pthread_barrier_wait(&barrier);
    sched_yield();

    shared_node->magic = 0xBADC0DE;
    free((void *)shared_node);
    shared_node = NULL;

    return NULL;
}

void *reader(void *arg) {
    (void)arg;
    pthread_barrier_wait(&barrier);

    if (shared_node != NULL) {
        assert(shared_node->magic == 0xDEADBEEF && "RUN!!! use-after-free detected");
        printf("Value read: %d\n", shared_node->value); /* potential UAF */
    }

    return NULL;
}

int main(void) {
    pthread_t t1, t2;
    pthread_barrier_init(&barrier, NULL, 2);

    pthread_create(&t1, NULL, writer, NULL);
    pthread_create(&t2, NULL, reader, NULL);

    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    pthread_barrier_destroy(&barrier);
    return 0;
}
