#define LIBDL 1

#include "stdlib.c"
#include <dlfcn.h>

#define RED "\33[31m"
#define OFF "\33[0m"

unsigned READ = 0x0;
unsigned WRITE = 0x1;
unsigned READ_WRITE = 0x2;

typedef void (*schedule_memop_t)(const void *, const void *, size_t, unsigned);
static schedule_memop_t schedule_memop_fn = NULL;

typedef void (*io_prepare_t)(const void *, long, long *);
static io_prepare_t io_prepare_fn = NULL;

void mem_wri(const void *instr_addr, const void *mem_addr, size_t size)
{
    if (schedule_memop_fn == NULL)
        return;
    dlcall(schedule_memop_fn, instr_addr, mem_addr, size, READ_WRITE);
}


void mem_ri(const void *instr_addr, const void *mem_addr, size_t size)
{
    if (schedule_memop_fn == NULL)
        return;
    dlcall(schedule_memop_fn, instr_addr, mem_addr, size, READ);
}

void mem_wi(const void *instr_addr, const void *mem_addr, size_t size)
{
    if (schedule_memop_fn == NULL)
        return;
    dlcall(schedule_memop_fn, instr_addr, mem_addr, size, WRITE);
}

/* Runs before each raw syscall instruction. rsched turns file and socket
 * syscalls into scheduling points and waits until they cannot block; the
 * original instruction then executes. rsched may shorten a poll timeout
 * (the third argument), so that register is passed by pointer. */
void io_syscall(const void *instr_addr, long nr, long a0, long a1, long *a2, long a3,
                long a4, long a5)
{
    if (io_prepare_fn == NULL)
        return;
    long args[6] = {a0, a1, *a2, a3, a4, a5};
    dlcall(io_prepare_fn, instr_addr, nr, args);
    *a2 = args[2];
}

void init(int argc, const char **argv, char **envp, void *dynp)
{
    environ = envp;
    dlinit(dynp);
    if (dlopen_impl == NULL || dlsym_impl == NULL)
        return;

    schedule_memop_fn = dlsym(NULL, "rsched_atomic_instrument_ra");
    io_prepare_fn = dlsym(NULL, "rsched_io_prepare_syscall");
}
