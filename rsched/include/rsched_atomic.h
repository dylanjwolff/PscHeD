#pragma once
/*
 * rsched_atomic.h — drop-in replacement for <stdatomic.h> that turns every
 * atomic operation into a cooperative scheduling point.
 *
 * Usage
 * -----
 * Replace the standard headers with this one (or the reverse):
 *
 *   #ifdef RSCHED
 *   #  include "rsched_atomic.h"   // rsched-aware: each atomic op yields
 *   #else
 *   #  include <stdatomic.h>       // real atomics, standard pthread/sched
 *   #  include <pthread.h>
 *   #  include <sched.h>
 *   #endif
 *
 * Everything else in the source — _Atomic declarations, atomic_load_explicit,
 * atomic_store_explicit, memory_order_* constants — stays identical.
 *
 * How it works
 * ------------
 * atomic_load_explicit and atomic_store_explicit are overridden as _Generic
 * macros that dispatch to rsched_atomic_load/store_{i32,u32,ptr} based on the
 * pointer type.  Each rsched function calls rsched_sched_yield() before
 * performing the actual atomic access, inserting a scheduling point between
 * the yield and the operation.
 *
 * The memory_order argument is accepted but ignored (rsched always uses
 * sequential consistency internally).
 *
 * This header also includes rsched.h, which #define-redirects pthread_create,
 * pthread_join, pthread_mutex_*, pthread_barrier_*, and sched_yield to their
 * rsched_* counterparts — so no other source changes are needed.
 */

#include "rsched.h"
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── memory_order ──────────────────────────────────────────────────────── */
/* Same numeric values as C11 stdatomic.h on GCC/Clang. */
typedef enum memory_order {
    memory_order_relaxed = 0,
    memory_order_consume = 1,
    memory_order_acquire = 2,
    memory_order_release = 3,
    memory_order_acq_rel = 4,
    memory_order_seq_cst = 5
} memory_order;

/* ── rsched_atomic_* function declarations ─────────────────────────────── */
/* Implemented in the rsched Rust library.  Each yields before the access. */

int          rsched_atomic_load_i32 (_Atomic int *ptr);
void         rsched_atomic_store_i32(_Atomic int *ptr, int val);

unsigned int rsched_atomic_load_u32 (_Atomic unsigned int *ptr);
void         rsched_atomic_store_u32(_Atomic unsigned int *ptr, unsigned int val);

/* ptr/val are void * so the _Generic default: arm needs no cast in the macro. */
void        *rsched_atomic_load_ptr (void *ptr);
void         rsched_atomic_store_ptr(void *ptr, void *val);

#ifdef __cplusplus
}
#endif

/* ── atomic_load_explicit / atomic_store_explicit overrides ────────────── */
/*
 * _Generic dispatches on the type of the pointer argument:
 *   _Atomic int *          → rsched_atomic_{load,store}_i32
 *   _Atomic unsigned int * → rsched_atomic_{load,store}_u32
 *   default (T * _Atomic *)→ rsched_atomic_{load,store}_ptr  (void * erased)
 *
 * The load default: arm returns void *, which in C implicitly converts to any
 * pointer type at the assignment site — no cast required in user code.
 * The store default: arm casts val through uintptr_t to silence pointer-int
 * conversion warnings when val is a typed pointer.
 */
/* Explicit casts in every arm prevent GCC/Clang from emitting type warnings
 * for the non-selected branches (compilers analyse all arms even though only
 * one is evaluated at run time). */
#define atomic_load_explicit(ptr, order)                                        \
    _Generic((ptr),                                                             \
        _Atomic int *:          rsched_atomic_load_i32((_Atomic int *)(ptr)),   \
        _Atomic unsigned int *: rsched_atomic_load_u32((_Atomic unsigned int *)(ptr)), \
        default:                rsched_atomic_load_ptr((void *)(ptr))           \
    )

#define atomic_store_explicit(ptr, val, order)                                  \
    _Generic((ptr),                                                             \
        _Atomic int *:          rsched_atomic_store_i32((_Atomic int *)(ptr),   \
                                    (int)(intptr_t)(val)),                      \
        _Atomic unsigned int *: rsched_atomic_store_u32((_Atomic unsigned int *)(ptr), \
                                    (unsigned int)(uintptr_t)(val)),            \
        default:                rsched_atomic_store_ptr((void *)(ptr),          \
                                    (void *)(uintptr_t)(val))                   \
    )

#define atomic_load(ptr)             atomic_load_explicit((ptr), memory_order_seq_cst)
#define atomic_store(ptr, desired)   atomic_store_explicit((ptr), (desired), memory_order_seq_cst)
