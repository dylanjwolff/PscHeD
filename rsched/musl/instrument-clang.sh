#!/usr/bin/env bash
set -euo pipefail

export RSCHED_LIBC_MODE=musl
exec "$RSCHED_WORKSPACE/libc-instrumentation/instrument-clang.sh" "$@"
