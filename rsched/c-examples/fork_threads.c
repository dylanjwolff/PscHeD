#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef RSCHED
#include "rsched.h"
#endif

struct shared_state {
    int ready;
    int last;
};

struct task_arg {
    struct shared_state *shared;
    int value;
};

static void run_task(struct shared_state *shared, int value) {
    __atomic_add_fetch(&shared->ready, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(&shared->ready, __ATOMIC_SEQ_CST) < 4)
        sched_yield();

    sched_yield();
    shared->last = value;
    sched_yield();
}

static void *thread_main(void *raw) {
    struct task_arg *arg = (struct task_arg *)raw;
    run_task(arg->shared, arg->value);
    return NULL;
}

static int run_process(struct shared_state *shared, int main_value, int thread_value) {
    pthread_t thread;
    struct task_arg arg = {
        .shared = shared,
        .value = thread_value,
    };
    int r = pthread_create(&thread, NULL, thread_main, &arg);
    if (r != 0) {
        fprintf(stderr, "pthread_create failed: %d\n", r);
        return 2;
    }

    run_task(shared, main_value);

    r = pthread_join(thread, NULL);
    if (r != 0) {
        fprintf(stderr, "pthread_join failed: %d\n", r);
        return 2;
    }
    return 0;
}

int main(void) {
    struct shared_state *shared = mmap(0, sizeof(*shared), PROT_READ | PROT_WRITE,
                                       MAP_ANONYMOUS | MAP_SHARED, -1, 0);
    if (shared == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    shared->ready = 0;
    shared->last = 0;

    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 2;
    }

    if (child == 0) {
        int r = run_process(shared, 3, 4);
        exit(r);
    }

    int r = run_process(shared, 1, 2);
    if (r != 0)
        return r;

    int status = 0;
    for (;;) {
        pid_t w = waitpid(child, &status, WNOHANG);
        if (w == child)
            break;
        if (w < 0 && errno != EINTR) {
            perror("waitpid");
            return 2;
        }
        sched_yield();
    }

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child failed: status=%d\n", status);
        return 1;
    }

    printf("ready=%d last=%d\n", __atomic_load_n(&shared->ready, __ATOMIC_SEQ_CST), shared->last);
    return shared->ready == 4 && shared->last >= 1 && shared->last <= 4 ? 0 : 1;
}
