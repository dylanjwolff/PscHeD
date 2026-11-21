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

#elif defined(CASE_SECCOMP_SIGNALS_BLOCKED)

/* Workers block every signal, as glibc does around thread creation and exit,
   then contend for a mutex, so rsched's own futexes trap while the program
   asked for SIGSYS to be blocked. The trap must still reach the handler. */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int counter;

static void *worker(void *arg) {
    (void)arg;
    sigset_t all;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    for (int i = 0; i < 1000; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

int main(void) {
    rsched_init();
    pthread_t threads[3];
    for (int i = 0; i < 3; i++)
        pthread_create(&threads[i], NULL, worker, NULL);
    for (int i = 0; i < 3; i++)
        pthread_join(threads[i], NULL);
    printf("counter = %d\n", counter);
    return counter == 3000 ? 0 : 1;
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

#elif defined(CASE_SECCOMP_RAW_FORK)

#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* A fork syscall instruction in the program, which neither rsched_fork nor
 * rsched_syscall sees: seccomp must route it through the scheduler, so the
 * child runs as a scheduled process. */
static long raw_fork(void) {
    long ret;
    __asm__ __volatile__("syscall"
                         : "=a"(ret)
                         : "a"((long)SYS_fork)
                         : "rcx", "r11", "memory");
    return ret;
}

int main(void) {
    int fds[2];
    char byte = 0;
    int status = 0;

    rsched_init();
    if (pipe(fds) != 0)
        return 2;
    long pid = raw_fork();
    if (pid < 0)
        return 3;
    if (pid == 0) {
        byte = 'k';
        write(fds[1], &byte, 1);
        rsched_process_exit();
        _exit(7);
    }
    if (read(fds[0], &byte, 1) != 1 || byte != 'k')
        return 4;
    if (waitpid((pid_t)pid, &status, 0) != pid || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 7)
        return 5;
    printf("child exited 7\n");
    return 0;
}

#else
#error "define one seccomp CASE_* variant"
#endif
