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

void init(int argc, const char **argv, char **envp, void *dynp)
{
    environ = envp;
    dlinit(dynp);
    if (dlopen_impl == NULL || dlsym_impl == NULL)
        return;

    schedule_memop_fn = dlsym(NULL, "rsched_atomic_instrument_ra");
}
