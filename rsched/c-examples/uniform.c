/* Port of zigsched/toy-examples/uniform.c
 *
 * Two threads race to bit-shift a shared integer, yielding between every
 * operation.  Thread 1 shifts left (×2); Thread 2 shifts left and adds 1.
 * A barrier synchronises the start.  The resulting value is appended to
 * dist.txt so the distribution of outcomes can be inspected.
 *
 * Compile: see c-examples/Makefile
 */
#include "rsched.h"
#include <stdio.h>
#include <stdlib.h>

#define NUM_THREADS 2

int x = 0;

pthread_mutex_t mutex   = PTHREAD_MUTEX_INITIALIZER;
pthread_cond_t  cond    = PTHREAD_COND_INITIALIZER;
pthread_barrier_t bar;

void *thread1(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    sched_yield();
    x = (x << 1);
    sched_yield();
    x = (x << 1);
    sched_yield();
    x = (x << 1);
    sched_yield();
    x = (x << 1);
    sched_yield();
    x = (x << 1);
    return NULL;
}

void *thread2(void *arg) {
    (void)arg;
    pthread_barrier_wait(&bar);
    sched_yield();
    x = (x << 1) + 1;
    sched_yield();
    x = (x << 1) + 1;
    sched_yield();
    x = (x << 1) + 1;
    sched_yield();
    x = (x << 1) + 1;
    sched_yield();
    x = (x << 1) + 1;
    return NULL;
}

int main(void) {
    pthread_t threads[NUM_THREADS];
    pthread_barrier_init(&bar, NULL, 2);

    pthread_create(&threads[0], NULL, thread1, NULL);
    pthread_create(&threads[1], NULL, thread2, NULL);

    pthread_join(threads[0], NULL);
    pthread_join(threads[1], NULL);

    int value = x;
    FILE *file = fopen("dist.txt", "a");
    fprintf(file, "%d\n", value);
    fclose(file);

    pthread_mutex_destroy(&mutex);
    pthread_cond_destroy(&cond);
    return 0;
}
