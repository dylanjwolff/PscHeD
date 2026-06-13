#!/usr/bin/env bash
set -euo pipefail

export RSCHED_LIBC_MODE=glibc
exec "$RSCHED_WORKSPACE/libc-instrumentation/instrument-clang.sh" "$@"
