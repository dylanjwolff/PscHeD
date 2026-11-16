# rsched GCC pass

This plugin is used only when building the instrumented glibc artifact.  It
keeps glibc on GCC while preserving the rsched libc instrumentation semantics
that cannot be implemented with source annotations alone.

The plugin rewrites GIMPLE calls that are specific to task creation, currently
`__clone` and `__clone_internal`, to the rsched runtime entry points.  The
glibc compiler driver performs the matching object-level symbol wrapping for
pthread/scheduler/process primitives after GCC emits each object.
