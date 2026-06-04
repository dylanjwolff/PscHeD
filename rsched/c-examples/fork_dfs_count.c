#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef RSCHED
#include "rsched_atomic.h"
#else
#include <stdatomic.h>
#endif

#define N_OPS 1

struct shared_trace {
    _Atomic int len;
    char trace[N_OPS * 2 + 1];
};

static void record_ops(struct shared_trace *shared, char tag) {
    for (int i = 0; i < N_OPS; i++) {
        int pos = atomic_fetch_add_explicit(&shared->len, 1, memory_order_seq_cst);
        if (pos >= 0 && pos < N_OPS * 2) {
            shared->trace[pos] = tag;
        }
    }
}

static int run_one(char out[N_OPS * 2 + 1]) {
    rsched_reinit(0);

    struct shared_trace *shared = mmap(0, sizeof(*shared), PROT_READ | PROT_WRITE,
                                       MAP_ANONYMOUS | MAP_SHARED, -1, 0);
    if (shared == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    atomic_store_explicit(&shared->len, 0, memory_order_relaxed);
    for (int i = 0; i < N_OPS * 2 + 1; i++)
        shared->trace[i] = 0;

    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 2;
    }

    if (child == 0) {
        record_ops(shared, 'C');
#ifdef RSCHED
        rsched_process_exit();
#endif
        _exit(0);
    }

    record_ops(shared, 'P');

    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) {
            perror("waitpid");
            return 2;
        }
    }

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child failed: status=%d\n", status);
        return 1;
    }
    int len = atomic_load_explicit(&shared->len, memory_order_relaxed);
    if (len != N_OPS * 2) {
        fprintf(stderr, "bad trace length: %d\n", len);
        return 1;
    }

    for (int i = 0; i < N_OPS * 2; i++)
        out[i] = shared->trace[i];
    out[N_OPS * 2] = 0;
    return 0;
}

static int trace_bit(const char *trace) {
    if (!strcmp(trace, "PC"))
        return 1 << 0;
    if (!strcmp(trace, "CP"))
        return 1 << 1;
    return 0;
}

int main(void) {
    setenv("RSCHED_SCHEDULER", "dfs", 1);
    rsched_dfs_reset();

    int mask = 0;
    size_t runs = 0;
    while (rsched_dfs_has_next()) {
        char trace[N_OPS * 2 + 1];
        int r = run_one(trace);
        if (r != 0)
            return r;
        int bit = trace_bit(trace);
        if (bit == 0) {
            fprintf(stderr, "unexpected trace: %s\n", trace);
            return 1;
        }
        mask |= bit;
        rsched_dfs_finish_current();
        runs++;
        if (runs > 1024) {
            fprintf(stderr, "too many dfs runs\n");
            return 1;
        }
    }

    printf("runs=%zu completed=%zu mask=0x%x\n", runs,
           rsched_dfs_completed_schedules(), mask);
    return mask == 0x3 && runs == rsched_dfs_completed_schedules() ? 0 : 1;
}
