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
