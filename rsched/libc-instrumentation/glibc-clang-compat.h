#pragma once

/*
 * glibc assumes that a compiler advertising GNU C support recognizes
 * _Float128 as a built-in type. Clang 17 supports the ABI-compatible
 * __float128 extension but not the _Float128 spelling.
 */
#ifdef __clang__
typedef __float128 _Float128;

/*
 * Clang rejects glibc's late asm labels. Emit equivalent temporary aliases;
 * the rsched LLVM pass normalizes them to glibc's __GI_* definition layout.
 */
#ifdef NO_HIDDEN
#undef hidden_proto
#undef hidden_tls_proto
#define __RSCHED_CONCAT_INNER(left, right) left##right
#define __RSCHED_CONCAT(left, right) __RSCHED_CONCAT_INNER(left, right)
#define __RSCHED_HIDDEN_MARKER_INNER(counter, name)                       \
    __rsched_hidden_ref_##counter##_##name
#define __RSCHED_HIDDEN_MARKER(counter, name)                             \
    __RSCHED_HIDDEN_MARKER_INNER(counter, name)
#define __RSCHED_HIDDEN_REF(name, counter)                                \
    static const char __RSCHED_HIDDEN_MARKER(counter, name)                \
        __attribute__((used)) = 0;
#define hidden_proto(name, attrs...)                                     \
    extern __typeof(name) __GI_##name                                    \
        __attribute__((visibility("hidden"), ##attrs));                   \
    __RSCHED_HIDDEN_REF(name, __COUNTER__)
#define hidden_tls_proto(name, attrs...)                                 \
    extern __thread __typeof(name) __GI_##name                           \
        __attribute__((visibility("hidden"), ##attrs));

#define __hidden_ver1(local, internal, name)                             \
    extern __typeof(name) internal                                       \
        __attribute__((alias(#local), visibility("hidden")));

#undef hidden_def
#undef hidden_weak
#undef hidden_ver
#undef hidden_data_def
#undef hidden_data_weak
#undef hidden_data_ver
#undef hidden_tls_def
#define hidden_def(name) strong_alias(name, __GI_##name);
#define hidden_weak(name) weak_alias(name, __GI_##name);
#define hidden_ver(local, name) strong_alias(local, __GI_##name);
#define hidden_data_def(name) strong_alias(name, __GI_##name);
#define hidden_data_weak(name) hidden_data_def(name)
#define hidden_data_ver(local, name) strong_alias(local, __GI_##name);
#define hidden_tls_def(name) hidden_data_def(name)

#undef hidden_nolink
#define hidden_nolink(name, lib, version)                                 \
    extern __typeof(name) __GI_##name                                     \
        __attribute__((alias(#name), visibility("hidden")));              \
    __RSCHED_HIDDEN_NOLINK_1(                                             \
        name, __EI_##name, name, VERSION_##lib##_##version)
#define __RSCHED_HIDDEN_NOLINK_1(local, internal, name, version)           \
    __RSCHED_HIDDEN_NOLINK_2(local, internal, name, version)
#define __RSCHED_HIDDEN_NOLINK_2(local, internal, name, version)           \
    extern __typeof(name) internal __attribute__((alias(#local)));         \
    __asm__(".symver " #internal ", " #name "@" #version);

#undef libc_hidden_builtin_def
#undef libc_hidden_builtin_weak
#undef libc_hidden_builtin_ver
#define libc_hidden_builtin_def(name)
#define libc_hidden_builtin_weak(name)
#define libc_hidden_builtin_ver(local, name)
#endif
#endif
