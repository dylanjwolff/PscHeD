#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
export RSCHED_WORKSPACE=${RSCHED_WORKSPACE:-"$(dirname "$script_dir")"}
export RSCHED_LIBC_MODE=glibc
exec "$RSCHED_WORKSPACE/libc-instrumentation/instrument-clang.sh" "$@"
