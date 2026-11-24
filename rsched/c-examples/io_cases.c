/* Message passing over files and sockets under exhaustive DFS.
 *
 * Each case exchanges messages between two tasks with blocking descriptors.
 * Every schedule must complete with the right messages: an unmodeled
 * blocking read would hang the only running task. Each schedule also records
 * a short trace, and DFS must reach EXPECTED_TRACES distinct traces. Build one
 * case with -DCASE_IO_<NAME>. Without -DRSCHED the program runs once on the
 * native scheduler.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef RSCHED
#include "rsched.h"
#endif

#define MAX_RUNS 4096

#define TRACE_MAX 4

/* Order of events in one schedule. Lives in shared memory so a forked child
   can record into it. */
struct trace {
    int len;
    char order[TRACE_MAX + 1];
};

static struct trace *trace;

static void mark(char who) {
    if (trace->len < TRACE_MAX)
        trace->order[trace->len] = who;
    trace->len++;
}

static int fail(const char *what) {
    fprintf(stderr, "%s: %s\n", what, strerror(errno));
    return 2;
}

/* Records whether data was already waiting on fd ('F') or the reader is
   about to block on an empty descriptor ('E'). FIONREAD is not a scheduling
   point, so the check itself does not change the schedule. */
static __attribute__((unused)) void note_pending(int fd) {
    int pending = 0;
    ioctl(fd, FIONREAD, &pending);
    mark(pending ? 'F' : 'E');
}

/* Reads exactly len bytes, so short reads on stream sockets cannot fail a
   correct schedule. */
static __attribute__((unused)) int read_full(int fd, char *buf, size_t len, int use_recv) {
    size_t done = 0;
    while (done < len) {
        ssize_t n = use_recv ? recv(fd, buf + done, len - done, 0)
                             : read(fd, buf + done, len - done);
        if (n <= 0)
            return -1;
        done += (size_t)n;
    }
    return 0;
}

static __attribute__((unused)) int expect(const char *got, const char *want) {
    if (memcmp(got, want, strlen(want)) != 0) {
        fprintf(stderr, "expected %s, got %.4s\n", want, got);
        return 1;
    }
    return 0;
}

#if defined(CASE_IO_PIPE_THREADS)

/* Ping-pong between two threads over a pair of pipes. The trace records
   whether the worker found the request waiting ("F") or blocked on an empty
   pipe until the main thread wrote ("E"). */
#define EXPECTED_TRACES 2
static int to_worker[2], to_main[2];

static void *pipe_worker(void *arg) {
    (void)arg;
    char buf[4];
    note_pending(to_worker[0]);
    if (read_full(to_worker[0], buf, 4, 0) != 0 || expect(buf, "ping"))
        return (void *)1;
    if (write(to_main[1], "pong", 4) != 4)
        return (void *)1;
    return 0;
}

static int run_once(void) {
    if (pipe(to_worker) != 0 || pipe(to_main) != 0)
        return fail("pipe");
    pthread_t thread;
    if (pthread_create(&thread, 0, pipe_worker, 0) != 0)
        return fail("pthread_create");
    char buf[4];
    int bad = write(to_worker[1], "ping", 4) != 4 || read_full(to_main[0], buf, 4, 0) != 0 ||
              expect(buf, "pong");
    void *worker_result;
    pthread_join(thread, &worker_result);
    close(to_worker[0]);
    close(to_worker[1]);
    close(to_main[0]);
    close(to_main[1]);
    return bad || worker_result != 0;
}

#elif defined(CASE_IO_STDIO_PIPE_THREADS)

/* The pipe ping-pong through stdio. The rsched.h macros do not reach libc's
   own read and write calls, so only the seccomp filter (RSCHED_SECCOMP=1)
   models them; without it a reader's fgets blocks in the kernel. Trapped
   I/O only yields while it would block, without adding scheduling points of
   its own (stdio may hold FILE locks), so DFS sees a single schedule: the
   worker finds the request waiting and the main thread waits for the reply. */
#define EXPECTED_TRACES 1
static int to_worker[2], to_main[2];

static void *stdio_worker(void *arg) {
    (void)arg;
    FILE *in = fdopen(to_worker[0], "r");
    FILE *out = fdopen(to_main[1], "w");
    char line[8];
    note_pending(to_worker[0]);
    int bad = !in || !out || !fgets(line, sizeof line, in) || strcmp(line, "ping\n") != 0 ||
              fputs("pong\n", out) < 0 || fflush(out) != 0;
    if (in)
        fclose(in);
    if (out)
        fclose(out);
    return (void *)(long)bad;
}

static int run_once(void) {
    if (pipe(to_worker) != 0 || pipe(to_main) != 0)
        return fail("pipe");
    pthread_t thread;
    if (pthread_create(&thread, 0, stdio_worker, 0) != 0)
        return fail("pthread_create");
    FILE *out = fdopen(to_worker[1], "w");
    FILE *in = fdopen(to_main[0], "r");
    char line[8];
    int bad = !in || !out || fputs("ping\n", out) < 0 || fflush(out) != 0 ||
              !fgets(line, sizeof line, in) || strcmp(line, "pong\n") != 0;
    void *worker_result;
    pthread_join(thread, &worker_result);
    if (in)
        fclose(in);
    if (out)
        fclose(out);
    return bad || worker_result != 0;
}

#elif defined(CASE_IO_SOCKETPAIR_PROCESSES)

/* Ping-pong between a parent and a forked child over a UNIX socketpair: the
   child sends the request and the parent replies. The trace records whether
   the parent found the request waiting ("F") or blocked in recv until the
   child sent it ("E"). */
#define EXPECTED_TRACES 2

static int run_once(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0)
        return fail("socketpair");
    pid_t child = fork();
    if (child < 0)
        return fail("fork");
    if (child == 0) {
        char buf[4];
        close(sv[0]);
        int bad = send(sv[1], "ping", 4, 0) != 4 || read_full(sv[1], buf, 4, 1) != 0 ||
                  expect(buf, "pong");
#ifdef RSCHED
        rsched_process_exit();
#endif
        _exit(bad);
    }
    close(sv[1]);
    char buf[4];
    note_pending(sv[0]);
    int bad = read_full(sv[0], buf, 4, 1) != 0 || expect(buf, "ping") ||
              send(sv[0], "pong", 4, 0) != 4;
    int status = 0;
    while (waitpid(child, &status, 0) < 0)
        if (errno != EINTR)
            return fail("waitpid");
    close(sv[0]);
    return bad || !WIFEXITED(status) || WEXITSTATUS(status) != 0;
}

#elif defined(CASE_IO_TCP_THREADS)

/* A TCP client thread connects to the main thread over loopback, sends a
   request, and waits for the reply. The server polls before accepting. The
   trace records whether the request was waiting on the accepted connection
   ("F") or the server blocked in recv until the client sent it ("E"). */
#define EXPECTED_TRACES 2
static struct sockaddr_in server_addr;

static void *tcp_client(void *arg) {
    (void)arg;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0)
        return (void *)1;
    char buf[4];
    int bad = connect(fd, (struct sockaddr *)&server_addr, sizeof server_addr) != 0 ||
              send(fd, "ping", 4, 0) != 4 || read_full(fd, buf, 4, 1) != 0 ||
              expect(buf, "pong");
    close(fd);
    return (void *)(long)bad;
}

static int run_once(void) {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    if (listener < 0)
        return fail("socket");
    memset(&server_addr, 0, sizeof server_addr);
    server_addr.sin_family = AF_INET;
    server_addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof server_addr;
    if (bind(listener, (struct sockaddr *)&server_addr, sizeof server_addr) != 0 ||
        listen(listener, 1) != 0 ||
        getsockname(listener, (struct sockaddr *)&server_addr, &len) != 0)
        return fail("listen");

    pthread_t thread;
    if (pthread_create(&thread, 0, tcp_client, 0) != 0)
        return fail("pthread_create");
    struct pollfd pfd = {.fd = listener, .events = POLLIN};
    int bad = poll(&pfd, 1, -1) != 1;
    int conn = accept(listener, 0, 0);
    if (conn >= 0)
        note_pending(conn);
    char buf[4];
    bad = bad || conn < 0 || read_full(conn, buf, 4, 1) != 0 || expect(buf, "ping") ||
          send(conn, "pong", 4, 0) != 4;
    void *client_result;
    pthread_join(thread, &client_result);
    if (conn >= 0)
        close(conn);
    close(listener);
    return bad || client_result != 0;
}

#elif defined(CASE_IO_FILE_THREADS)

/* Two threads each append two records to the same regular file. Writes are
   the only scheduling points between a thread's records, so all six
   interleavings, such as "ABAB", appear only if each write is one. */
#define EXPECTED_TRACES 6
static int file_fd;

static void *file_writer(void *arg) {
    (void)arg;
    int bad = write(file_fd, "B", 1) != 1;
    bad |= write(file_fd, "B", 1) != 1;
    return (void *)(long)bad;
}

static int run_once(void) {
    char path[] = "/tmp/rsched-io-file-XXXXXX";
    file_fd = mkstemp(path);
    if (file_fd < 0)
        return fail("mkstemp");
    unlink(path);
    pthread_t thread;
    if (pthread_create(&thread, 0, file_writer, 0) != 0)
        return fail("pthread_create");
    int bad = write(file_fd, "A", 1) != 1;
    bad |= write(file_fd, "A", 1) != 1;
    void *writer_result;
    pthread_join(thread, &writer_result);
    char buf[TRACE_MAX] = {0};
    bad = bad || writer_result != 0 || pread(file_fd, buf, TRACE_MAX, 0) != TRACE_MAX;
    close(file_fd);
    for (int i = 0; i < TRACE_MAX; i++)
        mark(buf[i]);
    return bad;
}

#elif defined(CASE_IO_BLOCKED_WAITER)

/* A worker waits in poll and then read on an empty pipe while the main
   thread and a helper each yield BLOCKED_YIELDS times before the main thread
   writes. A blocked waiter is not runnable, so it adds no scheduling points
   while it waits; a waiter that busy-waited would yield again and again
   until the write. A yield runs another task when one can run,
   so the worker always reaches its wait before the write ("E"). */
#define EXPECTED_TRACES 1
#define BLOCKED_YIELDS 6
static int blocked_pipe[2];

static void *blocked_worker(void *arg) {
    (void)arg;
    char c;
    struct pollfd pfd = {.fd = blocked_pipe[0], .events = POLLIN};
    note_pending(blocked_pipe[0]);
    if (poll(&pfd, 1, -1) != 1 || read(blocked_pipe[0], &c, 1) != 1 || c != 'x')
        return (void *)1;
    return 0;
}

static void *yielding_helper(void *arg) {
    (void)arg;
    for (int i = 0; i < BLOCKED_YIELDS; i++)
        sched_yield();
    return 0;
}

static int run_once(void) {
    if (pipe(blocked_pipe) != 0)
        return fail("pipe");
    pthread_t worker, helper;
    if (pthread_create(&worker, 0, blocked_worker, 0) != 0 ||
        pthread_create(&helper, 0, yielding_helper, 0) != 0)
        return fail("pthread_create");
    for (int i = 0; i < BLOCKED_YIELDS; i++)
        sched_yield();
    int bad = write(blocked_pipe[1], "x", 1) != 1;
    void *worker_result;
    pthread_join(worker, &worker_result);
    pthread_join(helper, 0);
    close(blocked_pipe[0]);
    close(blocked_pipe[1]);
    return bad || worker_result != 0;
}

#elif defined(CASE_IO_SLEEPING_WRITER_PROCESS)

/* A forked child reads a pipe that the parent writes only after sleeping
   an hour in select(0, NULL, NULL, NULL, &tv), as APR's apr_sleep does.
   The sleep is a timed wait in virtual time, so it takes no real time.
   Once the child blocks in read,
   no task can run, and whichever process holds the turn must expire the
   parent's sleep instead of waiting in the kernel for the child's pipe. The
   child always blocks before the write ("E"). */
#define EXPECTED_TRACES 1

static int run_once(void) {
    int p[2];
    if (pipe(p) != 0)
        return fail("pipe");
    pid_t child = fork();
    if (child < 0)
        return fail("fork");
    if (child == 0) {
        char c;
        close(p[1]);
        note_pending(p[0]);
        int bad = read(p[0], &c, 1) != 1 || c != 'x';
#ifdef RSCHED
        rsched_process_exit();
#endif
        _exit(bad);
    }
    close(p[0]);
    struct timeval tv = {3600, 0};
    int bad = select(0, NULL, NULL, NULL, &tv) != 0 || write(p[1], "x", 1) != 1;
    int status = 0;
    while (waitpid(child, &status, 0) < 0)
        if (errno != EINTR)
            return fail("waitpid");
    close(p[1]);
    return bad || !WIFEXITED(status) || WEXITSTATUS(status) != 0;
}

#elif defined(CASE_IO_THREAD_EXIT_IN_CHILD)

/* A forked child starts a thread that exits at once while the child's main
   thread reads a pipe the parent writes. pthread_create does not switch,
   so the main thread always finds the pipe empty ("E") and blocks before
   the thread runs and exits ("T"). No task of the child can then run, so
   the exit passes the turn to the parent; once the parent's write makes
   the read ready, the turn must come back to the child. */
#define EXPECTED_TRACES 1

static void *exit_at_once(void *arg) {
    mark('T');
    return arg;
}

static int run_once(void) {
    int p[2];
    if (pipe(p) != 0)
        return fail("pipe");
    pid_t child = fork();
    if (child < 0)
        return fail("fork");
    if (child == 0) {
        char c;
        pthread_t thread;
        close(p[1]);
        int bad = pthread_create(&thread, 0, exit_at_once, 0) != 0;
        note_pending(p[0]);
        bad |= read(p[0], &c, 1) != 1 || c != 'x';
        bad |= pthread_join(thread, 0) != 0;
#ifdef RSCHED
        rsched_process_exit();
#endif
        _exit(bad);
    }
    close(p[0]);
    int bad = write(p[1], "x", 1) != 1;
    int status = 0;
    while (waitpid(child, &status, 0) < 0)
        if (errno != EINTR)
            return fail("waitpid");
    close(p[1]);
    return bad || !WIFEXITED(status) || WEXITSTATUS(status) != 0;
}

#elif defined(CASE_IO_EPOLL_THREADS)

/* A worker waits in epoll_wait with no timeout for a pipe the main thread
   writes, as Apache's event MPM listener waits for connections. A wait in
   the kernel would hold the only turn and hang. The trace records whether
   the worker found the byte waiting ("F") or blocked until the write
   ("E"). */
#define EXPECTED_TRACES 2
static int epoll_pipe[2];

static void *epoll_worker(void *arg) {
    (void)arg;
    char c;
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN}, out;
    if (ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, epoll_pipe[0], &ev) != 0)
        return (void *)1;
    note_pending(epoll_pipe[0]);
    int bad = epoll_wait(ep, &out, 1, -1) != 1 || read(epoll_pipe[0], &c, 1) != 1 || c != 'x';
    close(ep);
    return (void *)(long)bad;
}

static int run_once(void) {
    if (pipe(epoll_pipe) != 0)
        return fail("pipe");
    pthread_t worker;
    if (pthread_create(&worker, 0, epoll_worker, 0) != 0)
        return fail("pthread_create");
    int bad = write(epoll_pipe[1], "x", 1) != 1;
    void *worker_result;
    pthread_join(worker, &worker_result);
    close(epoll_pipe[0]);
    close(epoll_pipe[1]);
    return bad || worker_result != 0;
}

#else
#error "define one CASE_IO_* variant"
#endif

int main(void) {
    trace = mmap(0, sizeof *trace, PROT_READ | PROT_WRITE, MAP_ANONYMOUS | MAP_SHARED, -1, 0);
    if (trace == MAP_FAILED)
        return fail("mmap");

#ifdef RSCHED
    setenv("RSCHED_SCHEDULER", "dfs", 1);
    rsched_init(); /* installs the seccomp filter when RSCHED_SECCOMP=1 */
    rsched_dfs_reset();
    size_t runs = 0;
    char seen[MAX_RUNS][TRACE_MAX + 1];
    int distinct = 0;
    while (rsched_dfs_has_next()) {
        rsched_reinit(0);
        memset(trace, 0, sizeof *trace);
        int r = run_once();
        if (r != 0) {
            fprintf(stderr, "schedule %zu failed\n", runs);
            return r;
        }
        if (trace->len > TRACE_MAX) {
            fprintf(stderr, "trace too long: %d\n", trace->len);
            return 1;
        }
        int known = 0;
        for (int i = 0; i < distinct; i++)
            known |= strcmp(seen[i], trace->order) == 0;
        if (!known)
            strcpy(seen[distinct++], trace->order);
        rsched_dfs_finish_current();
        if (++runs >= MAX_RUNS) {
            fprintf(stderr, "too many dfs runs\n");
            return 1;
        }
    }
    printf("runs=%zu completed=%zu traces=%d:", runs, rsched_dfs_completed_schedules(),
           distinct);
    for (int i = 0; i < distinct; i++)
        printf(" %s", seen[i]);
    printf("\n");
    return distinct == EXPECTED_TRACES && runs == rsched_dfs_completed_schedules() ? 0 : 1;
#else
    int r = run_once();
    printf("trace=%s\n", trace->order);
    return r;
#endif
}
