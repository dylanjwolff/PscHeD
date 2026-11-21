/* No-op rsched hook for running the instrumented binary natively. Under the
 * rsched runner the instrumented libc is preloaded and its definition wins. */
#include <stddef.h>

void rsched_atomic_instrument(const void *ptr, size_t size, unsigned access)
{
    (void)ptr;
    (void)size;
    (void)access;
}
