#!/usr/bin/env bash
# Runs the stress test across DuckDB configurations that change how it uses
# threads: once natively, then under rsched for several seeds on each glibc
# provider, plus a repeat of the first seed to check that a seed replays
# exactly (the summary line counts conflicts, checkpoints, and interrupts).
#
#   matrix.sh [config...]      # default: every config below
#
# Environment: SEEDS (default "0 1 2 3"), SCALE (default 0.05), TIMEOUT
# seconds per rsched run (default 120), PROVIDERS (default "native coro"),
# LOG_DIR (default target/duckdb/matrix). Writes one log per run and prints
# a summary table.
set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
workspace=$(cd -- "$here/../.." && pwd)
binary=${BUILD_DIR:-"$workspace/target/duckdb/build"}/duckdb_stress
log_dir=${LOG_DIR:-"$workspace/target/duckdb/matrix"}
seeds=(${SEEDS:-0 1 2 3})
providers=(${PROVIDERS:-native coro})
scale=${SCALE:-0.05}
timeout=${TIMEOUT:-120}

# name|db_threads|DUCKDB_SETTINGS|DUCKDB_PRAGMAS|DUCKDB_PATH
configs=(
    "baseline|4|||"
    "threads1|1|||"
    "threads8|8|||"
    "external2|4|external_threads=2||"
    "partial|4|scheduler_process_partial=true||"
    "alloc_bg|4|allocator_background_threads=true||"
    "external_algos|4|max_memory=32MB|SET debug_force_external=true|"
    "unordered|4|preserve_insertion_order=false||"
    "immediate_txn|4|immediate_transaction_mode=true||"
    "tiny_checkpoint|4|checkpoint_threshold=1KB||"
    "verify_parallel|4||PRAGMA verify_parallelism|"
    "in_memory|4|||:memory:"
)

wanted=("$@")
mkdir -p "$log_dir"

# The newest artifact of each provider, resolved once so a rebuild during
# the run cannot mix two builds.
declare -A runners
for provider in "${providers[@]}"; do
    manifest=$(ls -t "$workspace"/target/rsched-artifacts/glibc/"$provider"/release/*/manifest.json 2>/dev/null | head -1)
    [[ -f "$manifest" ]] || { echo "missing $provider glibc artifact" >&2; exit 2; }
    runners[$provider]="$(dirname "$manifest")/$(jq -r .runner "$manifest")"
done

# run <log> <timeout> <command...>: prints "status seconds".
run() {
    local log=$1 limit=$2
    shift 2
    local start end
    start=$(date +%s.%N)
    timeout --kill-after=5s "${limit}s" "$@" >"$log" 2>&1
    local status=$?
    end=$(date +%s.%N)
    printf '%s %.1f\n' "$status" "$(echo "$end - $start" | bc)"
}

verdict() {
    case $1 in
        0) echo ok ;;
        124|137) echo hang ;;
        *) echo "exit$1" ;;
    esac
}

printf '%-16s %-14s %s\n' config native "$(printf '%-40s ' "${providers[@]}")"
for entry in "${configs[@]}"; do
    IFS='|' read -r name threads settings pragmas path <<<"$entry"
    if ((${#wanted[@]})) && [[ ! " ${wanted[*]} " == *" $name "* ]]; then
        continue
    fi
    export DUCKDB_SETTINGS=$settings DUCKDB_PRAGMAS=$pragmas DUCKDB_PATH=$path

    read -r status secs < <(run "$log_dir/$name.native.log" 5 "$binary" 1 "$threads")
    row=$(printf '%-16s %-14s' "$name" "$(verdict "$status") ${secs}s")

    for provider in "${providers[@]}"; do
        runner=${runners[$provider]}
        passed=0 results=() total=0
        for seed in "${seeds[@]}"; do
            log="$log_dir/$name.$provider.seed$seed.log"
            read -r status secs < <(RANDOM_SEED=$seed run "$log" "$timeout" "$runner" "$binary" "$scale" "$threads")
            ((status == 0)) && ((passed += 1))
            ((status != 0)) && results+=("s$seed:$(verdict "$status")")
            total=$(echo "$total + $secs" | bc)
        done
        # Same seed again: identical output means the run replayed exactly.
        first="$log_dir/$name.$provider.seed${seeds[0]}.log"
        again="$log_dir/$name.$provider.seed${seeds[0]}.again.log"
        RANDOM_SEED=${seeds[0]} run "$again" "$timeout" "$runner" "$binary" "$scale" "$threads" >/dev/null
        if cmp -s "$first" "$again"; then replay=same; else replay=DIFFERS; fi
        cell="$passed/${#seeds[@]} ok, avg $(echo "scale=1; $total / ${#seeds[@]}" | bc)s, replay $replay"
        ((${#results[@]})) && cell+=" [${results[*]}]"
        row+=$(printf ' %-40s' "$cell")
    done
    echo "$row"
done
