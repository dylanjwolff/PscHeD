#pragma once
#include <poll.h>
#include <pthread.h>
#include <fcntl.h>
#include <sched.h>
#include <semaphore.h>
#include <stdbool.h>
#include <stddef.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/file.h>
#include <sys/random.h>
#include <sys/select.h>
#include <sys/sem.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
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
time_t rsched_time(time_t *);
int  rsched_gettimeofday(struct timeval *, void *);

/* Stat calls report virtual file times: a successful write or truncate
   sets a file's mtime and ctime to the virtual time. */
struct statx;
int  rsched_stat(const char *, struct stat *);
int  rsched_lstat(const char *, struct stat *);
int  rsched_fstat(int, struct stat *);
int  rsched_fstatat(int, const char *, struct stat *, int);
int  rsched_statx(int, const char *, int, unsigned int, struct statx *);
int  rsched_ftruncate(int, off_t);
int  rsched_truncate(const char *, off_t);

int  rsched_sched_yield(void);
pid_t rsched_fork(void);
long rsched_syscall(long, ...);
void rsched_process_exit(void);
int rsched_execv(const char *, char *const []);
int rsched_execve(const char *, char *const [], char *const []);
pid_t rsched_waitpid(pid_t, int *, int);
int   rsched_kill(pid_t, int);
int   rsched_pthread_kill(pthread_t, int);

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
int rsched_ppoll(struct pollfd *, nfds_t, const struct timespec *, const sigset_t *);
int rsched_select(int, fd_set *, fd_set *, fd_set *, struct timeval *);
int rsched_pselect(int, fd_set *, fd_set *, fd_set *, const struct timespec *, const sigset_t *);
int rsched_epoll_wait(int, struct epoll_event *, int, int);
int rsched_epoll_pwait(int, struct epoll_event *, int, int, const sigset_t *);
int rsched_sigtimedwait(const sigset_t *, siginfo_t *, const struct timespec *);

/* Kernel file locks and SysV semaphores.  Each lock operation is a
   scheduling point; a task that cannot take a lock waits, not runnable,
   until some task releases a kernel lock. */
int rsched_fcntl(int, int, ...);
int rsched_flock(int, int);
int rsched_lockf(int, int, off_t);
int rsched_semop(int, struct sembuf *, size_t);
int rsched_semtimedop(int, struct sembuf *, size_t, const struct timespec *);
int rsched_semctl(int, int, int, ...);

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
/* Function-like where the name is also a struct or field name. */
#define time(t)                 rsched_time(t)
#define gettimeofday            rsched_gettimeofday
#define stat(p, b)              rsched_stat(p, b)
#define lstat                   rsched_lstat
#define fstat                   rsched_fstat
#define fstatat                 rsched_fstatat
#define statx(d, p, f, m, b)    rsched_statx(d, p, f, m, b)
#define ftruncate               rsched_ftruncate
#define truncate                rsched_truncate
#define pthread_barrier_init    rsched_pthread_barrier_init
#define pthread_barrier_wait    rsched_pthread_barrier_wait
#define sched_yield             rsched_sched_yield
#define fork                    rsched_fork
#define syscall                 rsched_syscall
#define execv                   rsched_execv
#define execve                  rsched_execve
#define waitpid                 rsched_waitpid
#define kill                    rsched_kill
#define pthread_kill            rsched_pthread_kill
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
#define ppoll                   rsched_ppoll
#define select                  rsched_select
#define pselect                 rsched_pselect
#define epoll_wait              rsched_epoll_wait
#define epoll_pwait             rsched_epoll_pwait
#define sigtimedwait            rsched_sigtimedwait
#define fcntl                   rsched_fcntl
#define flock(fd, op)           rsched_flock(fd, op)
#define lockf                   rsched_lockf
#define semop                   rsched_semop
#define semtimedop              rsched_semtimedop
#define semctl                  rsched_semctl
#define close                   rsched_close
#define close_range             rsched_close_range
#define closefrom               rsched_closefrom
#define dup2                    rsched_dup2
#define dup3                    rsched_dup3

#ifdef __cplusplus
}
#endif
