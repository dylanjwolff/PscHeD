#!/usr/bin/env bash
# Runs the DuckDB stress test natively or under rsched with the instrumented
# glibc, each run bounded by a timeout because incomplete modeling usually
# shows up as a hang.
#
#   run.sh native              # time one uninstrumented-libc run
#   run.sh rsched [seed...]    # one rsched run per seed (default: seed 0)
#
# Environment: SCALE (default 1), DB_THREADS (default 4), TIMEOUT seconds
# (default 5 native, 120 rsched), BUILD_DIR, GLIBC_ARTIFACT (manifest.json;
# default: the newest native release glibc artifact), and any RSCHED_* or
# RANDOM_SEED variable rsched reads.
set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
workspace=$(cd -- "$here/../.." && pwd)
binary=${BUILD_DIR:-"$workspace/target/duckdb/build"}/duckdb_stress
scale=${SCALE:-1}
db_threads=${DB_THREADS:-4}
mode=${1:-rsched}
shift || true

[[ -x "$binary" ]] || { echo "missing $binary; run targets/duckdb/build.sh" >&2; exit 2; }

if [[ "$mode" == native ]]; then
    start=$(date +%s.%N)
    timeout --kill-after=2s "${TIMEOUT:-5}s" "$binary" "$scale" "$db_threads"
    status=$?
    echo "native: exit $status in $(echo "$(date +%s.%N) - $start" | bc)s"
    exit "$status"
fi

manifest=${GLIBC_ARTIFACT:-$(ls -t "$workspace"/target/rsched-artifacts/glibc/native/release/*/manifest.json 2>/dev/null | head -1)}
[[ -f "$manifest" ]] || { echo "missing glibc artifact; run cargo xtask build libc glibc --provider native" >&2; exit 2; }
runner="$(dirname "$manifest")/$(jq -r .runner "$manifest")"

seeds=("$@")
((${#seeds[@]})) || seeds=(0)
worst=0
for seed in "${seeds[@]}"; do
    start=$(date +%s.%N)
    RANDOM_SEED=$seed timeout --kill-after=5s "${TIMEOUT:-120}s" \
        "$runner" "$binary" "$scale" "$db_threads"
    status=$?
    elapsed=$(echo "$(date +%s.%N) - $start" | bc)
    case $status in
        0) verdict=ok ;;
        124|137) verdict="TIMEOUT (likely hang)" ;;
        *) verdict="exit $status" ;;
    esac
    echo "rsched seed=$seed: $verdict in ${elapsed}s"
    ((status != 0)) && worst=$status
done
exit "$worst"
