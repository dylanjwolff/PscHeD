#pragma once
#include <pthread.h>
#include <sched.h>
#include <sys/wait.h>
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

int  rsched_sched_yield(void);
pid_t rsched_fork(void);
int rsched_execv(const char *, char *const []);
int rsched_execve(const char *, char *const [], char *const []);
pid_t rsched_waitpid(pid_t, int *, int);

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
#define pthread_barrier_init    rsched_pthread_barrier_init
#define pthread_barrier_wait    rsched_pthread_barrier_wait
#define sched_yield             rsched_sched_yield
#define fork                    rsched_fork
#define execv                   rsched_execv
#define execve                  rsched_execve
#define waitpid                 rsched_waitpid

#ifdef __cplusplus
}
#endif
