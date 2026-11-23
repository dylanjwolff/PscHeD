#pragma once
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <stdbool.h>
#include <stddef.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Initialise the scheduler (called automatically on first use, but you may
   call it explicitly before spawning any threads). */
void rsched_init(void);

/* Re-initialise with a new seed.  Call after all threads from a previous run
   have been joined.  Used by tests to run multiple independent scenarios. */
void rsched_reinit(unsigned long long seed);
void rsched_dfs_reset(void);
bool rsched_dfs_has_next(void);
void rsched_dfs_finish_current(void);
size_t rsched_dfs_completed_schedules(void);

int rsched_fuzzer_test_one_input(const unsigned char *, unsigned long,
                                 int (*)(const unsigned char *, unsigned long));

int  rsched_pthread_create(pthread_t *, const pthread_attr_t *,
                           void *(*)(void *), void *);
int  rsched_pthread_join(pthread_t, void **);
void rsched_pthread_exit(void *) __attribute__((noreturn));

int  rsched_pthread_mutex_lock(pthread_mutex_t *);
int  rsched_pthread_mutex_trylock(pthread_mutex_t *);
int  rsched_pthread_mutex_unlock(pthread_mutex_t *);

int  rsched_pthread_cond_wait(pthread_cond_t *, pthread_mutex_t *);
int  rsched_pthread_cond_signal(pthread_cond_t *);
int  rsched_pthread_cond_broadcast(pthread_cond_t *);

int  rsched_pthread_barrier_init(pthread_barrier_t *,
                                 const pthread_barrierattr_t *,
                                 unsigned int);
int  rsched_pthread_barrier_wait(pthread_barrier_t *);

int  rsched_pthread_cond_timedwait(pthread_cond_t *, pthread_mutex_t *,
                                   const struct timespec *);
int  rsched_pthread_cond_clockwait(pthread_cond_t *, pthread_mutex_t *, clockid_t,
                                   const struct timespec *);

/* Semaphores, rwlocks, and pthread_once.  Timed waits expire in virtual time:
   once no task can run, or after RSCHED_STARVATION_LIMIT scheduling points,
   and never before their deadline has passed. */
int  rsched_sem_wait(sem_t *);
int  rsched_sem_trywait(sem_t *);
int  rsched_sem_timedwait(sem_t *, const struct timespec *);
int  rsched_sem_clockwait(sem_t *, clockid_t, const struct timespec *);
int  rsched_sem_post(sem_t *);
int  rsched_pthread_rwlock_rdlock(pthread_rwlock_t *);
int  rsched_pthread_rwlock_wrlock(pthread_rwlock_t *);
int  rsched_pthread_rwlock_tryrdlock(pthread_rwlock_t *);
int  rsched_pthread_rwlock_trywrlock(pthread_rwlock_t *);
int  rsched_pthread_rwlock_timedrdlock(pthread_rwlock_t *, const struct timespec *);
int  rsched_pthread_rwlock_timedwrlock(pthread_rwlock_t *, const struct timespec *);
int  rsched_pthread_rwlock_clockrdlock(pthread_rwlock_t *, clockid_t,
                                       const struct timespec *);
int  rsched_pthread_rwlock_clockwrlock(pthread_rwlock_t *, clockid_t,
                                       const struct timespec *);
int  rsched_pthread_rwlock_unlock(pthread_rwlock_t *);
int  rsched_pthread_once(pthread_once_t *, void (*)(void));

/* Sleeps block the task in virtual time, like timed waits. */
int  rsched_nanosleep(const struct timespec *, struct timespec *);
int  rsched_clock_nanosleep(clockid_t, int, const struct timespec *, struct timespec *);
int  rsched_usleep(useconds_t);
unsigned int rsched_sleep(unsigned int);

/* Deterministic inputs: virtual time, a per-task CPU, and seeded randomness. */
int  rsched_clock_gettime(clockid_t, struct timespec *);
int  rsched_sched_getcpu(void);
ssize_t rsched_getrandom(void *, size_t, unsigned int);

int  rsched_sched_yield(void);
pid_t rsched_fork(void);
long rsched_syscall(long, ...);
void rsched_process_exit(void);
int rsched_execv(const char *, char *const []);
int rsched_execve(const char *, char *const [], char *const []);
pid_t rsched_waitpid(pid_t, int *, int);

/* File and socket operations.  Each is a scheduling point; on a blocking
   descriptor, the calling task blocks until the operation can proceed. */
ssize_t rsched_read(int, void *, size_t);
ssize_t rsched_write(int, const void *, size_t);
ssize_t rsched_readv(int, const struct iovec *, int);
ssize_t rsched_writev(int, const struct iovec *, int);
ssize_t rsched_recv(int, void *, size_t, int);
ssize_t rsched_recvfrom(int, void *, size_t, int, struct sockaddr *,
                        socklen_t *);
ssize_t rsched_recvmsg(int, struct msghdr *, int);
ssize_t rsched_send(int, const void *, size_t, int);
ssize_t rsched_sendto(int, const void *, size_t, int,
                      const struct sockaddr *, socklen_t);
ssize_t rsched_sendmsg(int, const struct msghdr *, int);
int rsched_accept(int, struct sockaddr *, socklen_t *);
int rsched_accept4(int, struct sockaddr *, socklen_t *, int);
int rsched_connect(int, const struct sockaddr *, socklen_t);
int rsched_poll(struct pollfd *, nfds_t, int);

/* Descriptor closing.  rsched's own descriptors survive these: closing one
   reports success, and dup2/dup3 onto one fail with EBADF. */
int rsched_close(int);
int rsched_close_range(unsigned int, unsigned int, int);
void rsched_closefrom(int);
int rsched_dup2(int, int);
int rsched_dup3(int, int, int);

/* Redirect standard pthread / sched calls to the scheduler wrappers so that
   existing C programs need only add `#include "rsched.h"`. */
#define pthread_create          rsched_pthread_create
#define pthread_join            rsched_pthread_join
#define pthread_exit            rsched_pthread_exit
#define pthread_mutex_lock      rsched_pthread_mutex_lock
#define pthread_mutex_trylock   rsched_pthread_mutex_trylock
#define pthread_mutex_unlock    rsched_pthread_mutex_unlock
#define pthread_cond_wait       rsched_pthread_cond_wait
#define pthread_cond_signal     rsched_pthread_cond_signal
#define pthread_cond_broadcast  rsched_pthread_cond_broadcast
#define pthread_cond_timedwait  rsched_pthread_cond_timedwait
#define pthread_cond_clockwait  rsched_pthread_cond_clockwait
#define sem_wait                rsched_sem_wait
#define sem_trywait             rsched_sem_trywait
#define sem_timedwait           rsched_sem_timedwait
#define sem_clockwait           rsched_sem_clockwait
#define sem_post                rsched_sem_post
#define pthread_rwlock_rdlock   rsched_pthread_rwlock_rdlock
#define pthread_rwlock_wrlock   rsched_pthread_rwlock_wrlock
#define pthread_rwlock_tryrdlock rsched_pthread_rwlock_tryrdlock
#define pthread_rwlock_trywrlock rsched_pthread_rwlock_trywrlock
#define pthread_rwlock_timedrdlock rsched_pthread_rwlock_timedrdlock
#define pthread_rwlock_timedwrlock rsched_pthread_rwlock_timedwrlock
#define pthread_rwlock_clockrdlock rsched_pthread_rwlock_clockrdlock
#define pthread_rwlock_clockwrlock rsched_pthread_rwlock_clockwrlock
#define pthread_rwlock_unlock   rsched_pthread_rwlock_unlock
#define pthread_once            rsched_pthread_once
#define nanosleep               rsched_nanosleep
#define clock_nanosleep         rsched_clock_nanosleep
#define usleep                  rsched_usleep
#define sleep                   rsched_sleep
#define clock_gettime           rsched_clock_gettime
#define sched_getcpu            rsched_sched_getcpu
#define getrandom               rsched_getrandom
#define pthread_barrier_init    rsched_pthread_barrier_init
#define pthread_barrier_wait    rsched_pthread_barrier_wait
#define sched_yield             rsched_sched_yield
#define fork                    rsched_fork
#define syscall                 rsched_syscall
#define execv                   rsched_execv
#define execve                  rsched_execve
#define waitpid                 rsched_waitpid
#define read                    rsched_read
#define write                   rsched_write
#define readv                   rsched_readv
#define writev                  rsched_writev
#define recv                    rsched_recv
#define recvfrom                rsched_recvfrom
#define recvmsg                 rsched_recvmsg
#define send                    rsched_send
#define sendto                  rsched_sendto
#define sendmsg                 rsched_sendmsg
#define accept                  rsched_accept
#define accept4                 rsched_accept4
#define connect                 rsched_connect
#define poll                    rsched_poll
#define close                   rsched_close
#define close_range             rsched_close_range
#define closefrom               rsched_closefrom
#define dup2                    rsched_dup2
#define dup3                    rsched_dup3

#ifdef __cplusplus
}
#endif
