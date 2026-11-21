/* Linked into the stress binary so it runs both natively and under the
 * instrumented glibc 2.35.
 *
 * The host toolchain (glibc 2.41) makes the static libstdc++ and DuckDB
 * reference symbols glibc 2.35 does not export: arc4random (GLIBC_2.36), the
 * C23 __isoc23_* parsing aliases, and new fmod/fmodf versions (GLIBC_2.38).
 * Local definitions satisfy them without versioned references. Each one calls
 * the old symbol through an asm label, because the 2.41 headers would
 * otherwise redirect the call back to the __isoc23_* name. */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/random.h>
#include <sys/types.h>

long real_strtol(const char *, char **, int) __asm__("strtol");
unsigned long real_strtoul(const char *, char **, int) __asm__("strtoul");
long long real_strtoll(const char *, char **, int) __asm__("strtoll");
unsigned long long real_strtoull(const char *, char **, int) __asm__("strtoull");
int real_vsscanf(const char *, const char *, va_list) __asm__("vsscanf");
double real_fmod(double, double);
float real_fmodf(float, float);
__asm__(".symver real_fmod, fmod@GLIBC_2.2.5");
__asm__(".symver real_fmodf, fmodf@GLIBC_2.2.5");

long __isoc23_strtol(const char *s, char **end, int base) { return real_strtol(s, end, base); }
unsigned long __isoc23_strtoul(const char *s, char **end, int base)
{
    return real_strtoul(s, end, base);
}
long long __isoc23_strtoll(const char *s, char **end, int base) { return real_strtoll(s, end, base); }
unsigned long long __isoc23_strtoull(const char *s, char **end, int base)
{
    return real_strtoull(s, end, base);
}

int __isoc23_sscanf(const char *s, const char *format, ...)
{
    va_list args;
    va_start(args, format);
    int result = real_vsscanf(s, format, args);
    va_end(args);
    return result;
}

double fmod(double x, double y) { return real_fmod(x, y); }
float fmodf(float x, float y) { return real_fmodf(x, y); }

uint32_t arc4random(void)
{
    uint32_t value = 0;
    while (getrandom(&value, sizeof value, 0) != (ssize_t)sizeof value)
        ;
    return value;
}
