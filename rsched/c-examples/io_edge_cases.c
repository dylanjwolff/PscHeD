/* Edge cases of blocking I/O waits, one per -DCASE_IO_EDGE_<NAME>. Each case
 * prints "ok" and exits 0 when rsched handles it; a hang or a crash fails it.
 */
#define _GNU_SOURCE
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <unistd.h>

#include "rsched.h"

static int fds[2];

static __attribute__((unused)) void *reader(void *arg) {
    (void)arg;
    char c;
    return (void *)(long)(read(fds[0], &c, 1) != 1 || c != 'x');
}

#if defined(CASE_IO_EDGE_EXIT_WITH_BLOCKED_READER)

/* The process exits while another of its threads waits on a pipe nobody
   writes, like a server thread in accept: the exit must not wait for it. */
int main(void) {
    if (pipe(fds) != 0)
        return 2;
    pthread_t thread;
    pthread_create(&thread, 0, reader, 0);
    sched_yield();
    sched_yield();
    printf("ok\n");
    return 0;
}

#elif defined(CASE_IO_EDGE_POLL_NO_FDS)

/* poll(NULL, 0, -1) sleeps forever without touching a descriptor array. */
static void *sleeper(void *arg) {
    (void)arg;
    poll(NULL, 0, -1);
    return 0;
}

int main(void) {
    pthread_t thread;
    pthread_create(&thread, 0, sleeper, 0);
    for (int i = 0; i < 4; i++)
        sched_yield();
    printf("ok\n");
    return 0;
}

#elif defined(CASE_IO_EDGE_NESTED_EPOLL)

/* Polls a pipe through user epolls nested 1 to 5 deep. rsched's own epolls
   count toward the kernel's nesting limit, so the deep ones cannot be
   watched and must still finish. */
static int top;

static void *nested_poller(void *arg) {
    (void)arg;
    struct pollfd pfd = {.fd = top, .events = POLLIN};
    return (void *)(long)(poll(&pfd, 1, -1) != 1);
}

int main(void) {
    for (int depth = 1; depth <= 5; depth++) {
        if (pipe(fds) != 0)
            return 2;
        int epolls[5];
        top = fds[0];
        for (int i = 0; i < depth; i++) {
            epolls[i] = epoll_create1(0);
            struct epoll_event event = {.events = EPOLLIN};
            if (epolls[i] < 0 || epoll_ctl(epolls[i], EPOLL_CTL_ADD, top, &event) != 0)
                return 2;
            top = epolls[i];
        }
        pthread_t thread;
        pthread_create(&thread, 0, nested_poller, 0);
        for (int i = 0; i < 4; i++)
            sched_yield();
        if (write(fds[1], "x", 1) != 1)
            return 1;
        void *result;
        pthread_join(thread, &result);
        if (result != 0) {
            fprintf(stderr, "poll at depth %d failed\n", depth);
            return 1;
        }
        for (int i = 0; i < depth; i++)
            close(epolls[i]);
        close(fds[0]);
        close(fds[1]);
    }
    printf("ok\n");
    return 0;
}

#elif defined(CASE_IO_EDGE_RESERVED_FDS_SURVIVE)

/* The program tries every way to close descriptors it did not open, as
   daemons do, including rsched's own: close, closefrom, close_range, dup2,
   dup3, and with RSCHED_SECCOMP=1 a raw close_range syscall instruction.
   rsched's memfd and shared epoll must stay open, so a later epoll cannot
   get their numbers, and a blocked read must still work. */
#include <errno.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>

/* Finds rsched's descriptors by what they point to. */
static int find_reserved(int reserved[2]) {
    int found = 0;
    for (int fd = 3; fd < 4096; fd++) {
        char path[64], target[PATH_MAX];
        snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
        ssize_t n = readlink(path, target, sizeof target - 1);
        if (n <= 0)
            continue;
        target[n] = 0;
        if (strstr(target, "rsched-process-shared") || strcmp(target, "anon_inode:[eventpoll]") == 0) {
            if (found < 2)
                reserved[found] = fd;
            found++;
        }
    }
    return found;
}

static long raw_close_range(long first, long last) {
    long ret;
    __asm__ __volatile__("syscall"
                         : "=a"(ret)
                         : "a"((long)SYS_close_range), "D"(first), "S"(last), "d"(0L)
                         : "rcx", "r11", "memory");
    return ret;
}

int main(void) {
    rsched_init();
    if (pipe(fds) != 0)
        return 2;
    int reserved[2];
    if (find_reserved(reserved) != 2) {
        fprintf(stderr, "expected rsched's 2 descriptors\n");
        return 2;
    }
    for (int fd = 3; fd < 4096; fd++)
        if (fd != fds[0] && fd != fds[1])
            close(fd);
    closefrom(fds[1] + 1);
    close_range(fds[1] + 1, ~0U, 0);
    for (int i = 0; i < 2; i++) {
        errno = 0;
        if (dup2(fds[0], reserved[i]) != -1 || errno != EBADF ||
            dup3(fds[0], reserved[i], 0) != -1 || errno != EBADF) {
            fprintf(stderr, "dup onto descriptor %d was allowed\n", reserved[i]);
            return 1;
        }
    }
    if (getenv("RSCHED_SECCOMP") && raw_close_range(fds[1] + 1, ~0U) != 0)
        return 1;

    int again[2];
    if (find_reserved(again) != 2 || again[0] != reserved[0] || again[1] != reserved[1]) {
        fprintf(stderr, "rsched's descriptors were closed\n");
        return 1;
    }
    pthread_t thread;
    pthread_create(&thread, 0, reader, 0);
    for (int i = 0; i < 4; i++)
        sched_yield();
    if (write(fds[1], "x", 1) != 1)
        return 1;
    void *result;
    pthread_join(thread, &result);
    if (result != 0)
        return 1;
    printf("ok\n");
    return 0;
}

#elif defined(CASE_IO_EDGE_BAD_FD)

/* I/O on descriptor -1 fails with EBADF, as glibc's own tests check. */
#include <errno.h>
#include <sys/socket.h>

int main(void) {
    rsched_init();
    char c;
    errno = 0;
    if (read(-1, &c, 1) != -1 || errno != EBADF)
        return 1;
    errno = 0;
    if (recvmsg(-1, NULL, 0) != -1 || errno != EBADF)
        return 1;
    printf("ok\n");
    return 0;
}

#elif defined(CASE_IO_EDGE_UNPARKED_THREAD_NEEDS_LOCK)

/* The only task waits on a pipe, so rsched waits in the kernel. A thread
   rsched does not schedule, like a free-running thread under TSAN, takes
   rsched's global lock and then writes the pipe: rsched must not hold the
   lock while it waits. */
#undef pthread_create
#undef write
#undef pthread_join
void rsched_note_pthread_mutexattr_init(pthread_mutexattr_t *attr);

static void *unscheduled_writer(void *arg) {
    (void)arg;
    usleep(100 * 1000); /* let the reader block first */
    pthread_mutexattr_t attr;
    rsched_note_pthread_mutexattr_init(&attr);
    return (void *)(long)(write(fds[1], "x", 1) != 1);
}

int main(void) {
    rsched_init();
    if (pipe(fds) != 0)
        return 2;
    pthread_t writer;
    pthread_create(&writer, 0, unscheduled_writer, 0);
    if (reader(0) != 0)
        return 1;
    pthread_join(writer, 0);
    printf("ok\n");
    return 0;
}

#else
#error "define one CASE_IO_EDGE_* variant"
#endif
