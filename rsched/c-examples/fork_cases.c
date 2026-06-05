#if defined(CASE_FORK_COUNTER)

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
        if (r == child)
            break;
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

#elif defined(CASE_FORK_THREADS)

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

#elif defined(CASE_FORK_EXECV)

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef RSCHED
#include "rsched.h"
#endif

struct shared_state {
    int last;
};

static int shared_fd_create(void) {
    int fd = (int)syscall(SYS_memfd_create, "rsched-execv-example", 0);
    if (fd < 0)
        return -1;
    if (ftruncate(fd, (off_t)sizeof(struct shared_state)) != 0)
        return -1;
    return fd;
}

static struct shared_state *map_shared(int fd) {
    void *p = mmap(0, sizeof(struct shared_state), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED)
        return NULL;
    return (struct shared_state *)p;
}

static void write_event(struct shared_state *shared, int value) {
    sched_yield();
    shared->last = value;
    sched_yield();
}

static int worker_main(const char *fd_arg) {
    int fd = atoi(fd_arg);
    struct shared_state *shared = map_shared(fd);
    if (!shared) {
        perror("worker mmap");
        return 2;
    }
    write_event(shared, 2);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "worker") == 0)
        return worker_main(argv[2]);

    int fd = shared_fd_create();
    if (fd < 0) {
        perror("memfd_create/ftruncate");
        return 2;
    }
    struct shared_state *shared = map_shared(fd);
    if (!shared) {
        perror("mmap");
        return 2;
    }
    shared->last = 0;

    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 2;
    }

    if (child == 0) {
        char fd_buf[32];
        snprintf(fd_buf, sizeof(fd_buf), "%d", fd);
        char *args[] = {argv[0], "worker", fd_buf, NULL};
        execv(argv[0], args);
        perror("execv");
        return 2;
    }

    write_event(shared, 1);

    int status = 0;
    for (;;) {
        pid_t r = waitpid(child, &status, WNOHANG);
        if (r == child)
            break;
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

    printf("last=%d\n", shared->last);
    return shared->last == 1 || shared->last == 2 ? 0 : 1;
}

#elif defined(CASE_FORK_DFS_COUNT)

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
        if (pos >= 0 && pos < N_OPS * 2)
            shared->trace[pos] = tag;
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

#else
#error "define one CASE_FORK_* variant"
#endif
