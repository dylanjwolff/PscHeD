#include "rsched.h"

#if defined(CASE_SECCOMP_OK)

#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int counter;

static void *worker(void *arg) {
    (void)arg;
    pthread_mutex_lock(&lock);
    counter++;
    pthread_mutex_unlock(&lock);
    return NULL;
}

int main(void) {
    rsched_init();

    pthread_t thread;
    pthread_create(&thread, NULL, worker, NULL);
    pthread_join(thread, NULL);

    printf("counter = %d\n", counter);
    return counter == 1 ? 0 : 1;
}

#elif defined(CASE_SECCOMP_RAW_FUTEX)

#include <linux/futex.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    int word = 0;

    rsched_init();
    syscall(SYS_futex, &word, FUTEX_WAKE, 1, 0, 0, 0);
    return 0;
}

#else
#error "define one seccomp CASE_* variant"
#endif
