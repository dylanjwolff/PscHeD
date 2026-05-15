#include <errno.h>
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
    volatile int counter;
    volatile int trace_len;
    char trace[16];
};

static void bump(struct shared_state *shared, char tag) {
    for (int i = 0; i < 3; i++) {
        int pos = shared->trace_len;
        if (pos >= 0 && pos < (int)sizeof(shared->trace)) {
            shared->trace[pos] = tag;
            shared->trace_len = pos + 1;
        }
        int value = shared->counter;
        sched_yield();
        shared->counter = value + 1;
        sched_yield();
    }
}

int main(void) {
    struct shared_state *shared = mmap(0, sizeof(*shared), PROT_READ | PROT_WRITE,
                                       MAP_ANONYMOUS | MAP_SHARED, -1, 0);
    if (shared == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    shared->counter = 0;
    shared->trace_len = 0;
    for (int i = 0; i < (int)sizeof(shared->trace); i++)
        shared->trace[i] = 0;

    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 2;
    }

    if (child == 0) {
        bump(shared, 'C');
        exit(0);
    }

    bump(shared, 'P');

    int status = 0;
    for (;;) {
        pid_t r = waitpid(child, &status, WNOHANG);
        if (r == child) {
            break;
        }
        if (r < 0 && errno != EINTR) {
            perror("waitpid");
            return 2;
        }
        sched_yield();
    }

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child failed: status=%d\n", status);
        return 1;
    }

    if (shared->trace_len < 0 || shared->trace_len > 6) {
        fprintf(stderr, "bad trace length: %d\n", shared->trace_len);
        return 1;
    }
    shared->trace[shared->trace_len] = 0;
    printf("counter=%d trace=%s\n", shared->counter, shared->trace);
    return shared->trace_len == 6 ? 0 : 1;
}
