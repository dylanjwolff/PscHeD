/*
 * glibc implements syscall in assembly, which cannot pass through an LLVM
 * module pass. The compiler driver substitutes this marker definition for
 * that one object; the rsched libc pass replaces it with the x86_64 tail-jump
 * trampoline to rsched_libc_syscall.
 */
long syscall(long number, ...) {
    return number;
}
