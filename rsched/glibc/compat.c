/* Symbols from glibc 2.36 to 2.38 that the instrumented glibc 2.35 lacks.
 *
 * Programs built against a newer host glibc reference them without asking:
 * the host headers redirect strtol, sscanf and friends to their C23
 * __isoc23_* names, and a host libstdc++ calls arc4random. The build links
 * this file into libc.so and exports each symbol under its upstream version,
 * so such programs load. The rest of the newer API (stdc_*, pidfd_*, ...) is
 * not provided, and the loader reports it as missing.
 *
 * The C23 functions differ from the old ones only in accepting binary input:
 * a 0b prefix for base 0 or 2, and the %b scanf conversion. The integer
 * parsers implement it. The scanf functions forward to the old ones, which
 * parse everything else identically, and abort when a C23-only input could
 * reach them rather than parse it the old way.
 *
 * This file includes no headers that declare the redirected names, so every
 * call below reaches the old symbol. */
#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <wctype.h>

typedef struct _IO_FILE FILE;
typedef struct __locale_struct *locale_t;

extern FILE *stdin;
long long strtoll(const char *, char **, int);
unsigned long long strtoull(const char *, char **, int);
long long strtoll_l(const char *, char **, int, locale_t);
unsigned long long strtoull_l(const char *, char **, int, locale_t);
long long wcstoll(const wchar_t *, wchar_t **, int);
unsigned long long wcstoull(const wchar_t *, wchar_t **, int);
long long wcstoll_l(const wchar_t *, wchar_t **, int, locale_t);
unsigned long long wcstoull_l(const wchar_t *, wchar_t **, int, locale_t);
int vsscanf(const char *, const char *, va_list);
int vfscanf(FILE *, const char *, va_list);
int vswscanf(const wchar_t *, const wchar_t *, va_list);
int vfwscanf(FILE *, const wchar_t *, va_list);
long getrandom(void *, size_t, unsigned);
size_t strlen(const char *);
void *memcpy(void *, const void *, size_t);
size_t wcslen(const wchar_t *);
void abort(void);
void __chk_fail(void) __attribute__((noreturn));
int dprintf(int, const char *, ...);

static __attribute__((noreturn)) void unsupported(const char *what)
{
    dprintf(2, "[rsched] unsupported C23 input in %s\n", what);
    abort();
}

/* ---- Integer parsing ---------------------------------------------------- */

/* Returns where the binary digits of "[space][sign]0b<digit>" start, or 0
   when the C23 prefix does not apply. */
#define BINARY_DIGITS(name, type, space)                                       \
    static const type *name(const type *s, int base, int *negative)            \
    {                                                                          \
        if (base != 0 && base != 2)                                            \
            return 0;                                                          \
        while (space(*s))                                                      \
            s++;                                                               \
        *negative = *s == '-';                                                 \
        if (*s == '-' || *s == '+')                                            \
            s++;                                                               \
        if (s[0] == '0' && (s[1] == 'b' || s[1] == 'B') &&                     \
            (s[2] == '0' || s[2] == '1'))                                      \
            return s + 2;                                                      \
        return 0;                                                              \
    }
BINARY_DIGITS(binary_digits, char, isspace)
BINARY_DIGITS(binary_wdigits, wchar_t, iswspace)

/* The digits parse as unsigned; the sign is applied as strtol and strtoul
   do, saturating with ERANGE outside the type. */
static long long signed_value(unsigned long long magnitude, int overflow, int negative)
{
    if (overflow)
        return negative ? LLONG_MIN : LLONG_MAX;
    if (!negative && magnitude > (unsigned long long)LLONG_MAX) {
        errno = ERANGE;
        return LLONG_MAX;
    }
    if (negative && magnitude > (unsigned long long)LLONG_MAX + 1) {
        errno = ERANGE;
        return LLONG_MIN;
    }
    return negative ? (long long)(0 - magnitude) : (long long)magnitude;
}

static unsigned long long unsigned_value(unsigned long long magnitude, int overflow,
                                         int negative)
{
    if (overflow)
        return ULLONG_MAX;
    return negative ? 0 - magnitude : magnitude;
}

#define PARSERS(type, signed_name, unsigned_name, digits, old_signed, old_unsigned, \
                ...)                                                           \
    long long signed_name(const type *s, type **end, int base __VA_OPT__(, ) \
                              __VA_ARGS__)                                     \
    {                                                                          \
        int negative;                                                          \
        const type *start = digits(s, base, &negative);                        \
        if (!start)                                                            \
            return old_signed(s, end, base PARSER_LOCALE(__VA_ARGS__));        \
        int saved = errno, overflow;                                           \
        errno = 0;                                                             \
        unsigned long long m = old_unsigned(start, end, 2 PARSER_LOCALE(__VA_ARGS__)); \
        overflow = errno == ERANGE;                                            \
        errno = overflow ? ERANGE : saved;                                     \
        return signed_value(m, overflow, negative);                            \
    }                                                                          \
    unsigned long long unsigned_name(const type *s, type **end,                \
                                     int base __VA_OPT__(, ) __VA_ARGS__)      \
    {                                                                          \
        int negative;                                                          \
        const type *start = digits(s, base, &negative);                        \
        if (!start)                                                            \
            return old_unsigned(s, end, base PARSER_LOCALE(__VA_ARGS__));      \
        int saved = errno, overflow;                                           \
        errno = 0;                                                             \
        unsigned long long m = old_unsigned(start, end, 2 PARSER_LOCALE(__VA_ARGS__)); \
        overflow = errno == ERANGE;                                            \
        errno = overflow ? ERANGE : saved;                                     \
        return unsigned_value(m, overflow, negative);                          \
    }
#define PARSER_LOCALE(...) __VA_OPT__(, locale)

PARSERS(char, __isoc23_strtoll, __isoc23_strtoull, binary_digits, strtoll, strtoull)
PARSERS(char, __isoc23_strtoll_l, __isoc23_strtoull_l, binary_digits, strtoll_l,
        strtoull_l, locale_t locale)
PARSERS(wchar_t, __isoc23_wcstoll, __isoc23_wcstoull, binary_wdigits, wcstoll, wcstoull)
PARSERS(wchar_t, __isoc23_wcstoll_l, __isoc23_wcstoull_l, binary_wdigits, wcstoll_l,
        wcstoull_l, locale_t locale)

/* long, long long and intmax_t are all 64 bits on x86_64. */
#define ALIAS(name, target) __typeof__(target) name __attribute__((alias(#target)));
ALIAS(__isoc23_strtol, __isoc23_strtoll)
ALIAS(__isoc23_strtoimax, __isoc23_strtoll)
ALIAS(__isoc23_strtoul, __isoc23_strtoull)
ALIAS(__isoc23_strtoumax, __isoc23_strtoull)
ALIAS(__isoc23_strtol_l, __isoc23_strtoll_l)
ALIAS(__isoc23_strtoul_l, __isoc23_strtoull_l)
ALIAS(__isoc23_wcstol, __isoc23_wcstoll)
ALIAS(__isoc23_wcstoimax, __isoc23_wcstoll)
ALIAS(__isoc23_wcstoul, __isoc23_wcstoull)
ALIAS(__isoc23_wcstoumax, __isoc23_wcstoull)
ALIAS(__isoc23_wcstol_l, __isoc23_wcstoll_l)
ALIAS(__isoc23_wcstoul_l, __isoc23_wcstoull_l)

/* ---- scanf -------------------------------------------------------------- */

/* Which conversions with a C23 meaning the format has: BINARY for %b, which
   the old scanf rejects, and PREFIXED for %i, which differs only on input
   with a 0b prefix. */
enum { PREFIXED = 1, BINARY = 2 };
#define BINARY_CONVERSION(name, type)                                          \
    static int name(const type *f)                                             \
    {                                                                          \
        int found = 0;                                                         \
        while (*f) {                                                           \
            if (*f++ != '%')                                                   \
                continue;                                                      \
            if (*f == '%') {                                                   \
                f++;                                                           \
                continue;                                                      \
            }                                                                  \
            while (*f && !(*f >= 'A' && *f <= 'Z' && *f != 'L') &&             \
                   !(*f >= 'a' && *f <= 'z' && *f != 'h' && *f != 'l' &&       \
                     *f != 'j' && *f != 'z' && *f != 't' && *f != 'q' &&       \
                     *f != 'm') &&                                             \
                   *f != '[')                                                  \
                f++;                                                           \
            if (*f == 'b')                                                     \
                found |= BINARY;                                               \
            if (*f == 'i')                                                     \
                found |= PREFIXED;                                             \
            if (*f == '[') {                                                   \
                f++;                                                           \
                if (*f == '^')                                                 \
                    f++;                                                       \
                if (*f == ']')                                                 \
                    f++;                                                       \
                while (*f && *f != ']')                                        \
                    f++;                                                       \
            }                                                                  \
            if (*f)                                                            \
                f++;                                                           \
        }                                                                      \
        return found;                                                          \
    }
BINARY_CONVERSION(binary_conversion, char)
BINARY_CONVERSION(binary_wconversion, wchar_t)

/* A string input without "0b" parses the same under both standards. */
#define HAS_BINARY_PREFIX(name, type)                                          \
    static int name(const type *s)                                             \
    {                                                                          \
        for (; *s; s++)                                                        \
            if (s[0] == '0' && (s[1] == 'b' || s[1] == 'B'))                   \
                return 1;                                                      \
        return 0;                                                              \
    }
HAS_BINARY_PREFIX(has_binary_prefix, char)
HAS_BINARY_PREFIX(has_binary_wprefix, wchar_t)

int __isoc23_vsscanf(const char *s, const char *format, va_list args)
{
    int found = binary_conversion(format);
    if ((found & BINARY) || ((found & PREFIXED) && has_binary_prefix(s)))
        unsupported("sscanf");
    return vsscanf(s, format, args);
}

int __isoc23_vswscanf(const wchar_t *s, const wchar_t *format, va_list args)
{
    int found = binary_wconversion(format);
    if ((found & BINARY) || ((found & PREFIXED) && has_binary_wprefix(s)))
        unsupported("swscanf");
    return vswscanf(s, format, args);
}

/* A stream's input is unknown until read. */
int __isoc23_vfscanf(FILE *stream, const char *format, va_list args)
{
    if (binary_conversion(format))
        unsupported("fscanf");
    return vfscanf(stream, format, args);
}

int __isoc23_vfwscanf(FILE *stream, const wchar_t *format, va_list args)
{
    if (binary_wconversion(format))
        unsupported("fwscanf");
    return vfwscanf(stream, format, args);
}

int __isoc23_vscanf(const char *format, va_list args)
{
    return __isoc23_vfscanf(stdin, format, args);
}

int __isoc23_vwscanf(const wchar_t *format, va_list args)
{
    return __isoc23_vfwscanf(stdin, format, args);
}

#define VARIADIC(name, forward, ...)                                           \
    int name(__VA_ARGS__, ...)                                                 \
    {                                                                          \
        va_list args;                                                          \
        va_start(args, format);                                                \
        int result = forward;                                                  \
        va_end(args);                                                          \
        return result;                                                         \
    }
VARIADIC(__isoc23_sscanf, __isoc23_vsscanf(s, format, args), const char *s,
         const char *format)
VARIADIC(__isoc23_swscanf, __isoc23_vswscanf(s, format, args), const wchar_t *s,
         const wchar_t *format)
VARIADIC(__isoc23_fscanf, __isoc23_vfscanf(stream, format, args), FILE *stream,
         const char *format)
VARIADIC(__isoc23_fwscanf, __isoc23_vfwscanf(stream, format, args), FILE *stream,
         const wchar_t *format)
VARIADIC(__isoc23_scanf, __isoc23_vscanf(format, args), const char *format)
VARIADIC(__isoc23_wscanf, __isoc23_vwscanf(format, args), const wchar_t *format)

/* ---- arc4random --------------------------------------------------------- */

/* getrandom is the instrumented libc's, so the bytes come from the seeded
   generator under rsched. */
void arc4random_buf(void *buffer, size_t size)
{
    unsigned char *p = buffer;
    while (size) {
        long n = getrandom(p, size, 0);
        if (n > 0) {
            p += n;
            size -= (size_t)n;
        } else if (n < 0 && errno != EINTR) {
            abort();
        }
    }
}

uint32_t arc4random(void)
{
    uint32_t value;
    arc4random_buf(&value, sizeof value);
    return value;
}

uint32_t arc4random_uniform(uint32_t bound)
{
    if (bound < 2)
        return 0;
    /* Reject the low values that would bias the remainder. */
    uint32_t minimum = (0 - bound) % bound;
    for (;;) {
        uint32_t value = arc4random();
        if (value >= minimum)
            return value % bound;
    }
}

/* ---- strlcpy and strlcat ------------------------------------------------ */

#define LCOPY(cpy, cat, cpy_chk, cat_chk, type, length)                        \
    size_t cpy(type *dest, const type *src, size_t size)                       \
    {                                                                          \
        size_t n = length(src);                                                \
        if (size) {                                                            \
            size_t copy = n < size ? n : size - 1;                             \
            memcpy(dest, src, copy * sizeof(type));                            \
            dest[copy] = 0;                                                    \
        }                                                                      \
        return n;                                                              \
    }                                                                          \
    size_t cat(type *dest, const type *src, size_t size)                       \
    {                                                                          \
        size_t d = 0;                                                          \
        while (d < size && dest[d])                                            \
            d++;                                                               \
        if (d == size)                                                         \
            return size + length(src);                                         \
        return d + cpy(dest + d, src, size - d);                               \
    }                                                                          \
    size_t cpy_chk(type *dest, const type *src, size_t size, size_t object)    \
    {                                                                          \
        if (object < size)                                                     \
            __chk_fail();                                                      \
        return cpy(dest, src, size);                                           \
    }                                                                          \
    size_t cat_chk(type *dest, const type *src, size_t size, size_t object)    \
    {                                                                          \
        if (object < size)                                                     \
            __chk_fail();                                                      \
        return cat(dest, src, size);                                           \
    }
LCOPY(strlcpy, strlcat, __strlcpy_chk, __strlcat_chk, char, strlen)
LCOPY(wcslcpy, wcslcat, __wcslcpy_chk, __wcslcat_chk, wchar_t, wcslen)
