#!/bin/bash
set -euo pipefail

# pushd e9patch; CC=gcc CXX=g++ ./build.sh; popd;

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
E9_DIR="${SCRIPT_DIR}/e9patch"

if [ $# -lt 1 ]; then
    echo "usage: $0 <binary> [e9tool options...]" >&2
    exit 2
fi

cp "${SCRIPT_DIR}/hooks/schedule_memops.c" "${E9_DIR}/examples"

BINARY=$(command -v "$1" || true)
if [ -z "${BINARY}" ]; then
    BINARY="$1"
fi
BINARY=$(readlink -f "${BINARY}")
BASENAME=$(basename "$BINARY")

OUT_DIR=${OUT_DIR:-"${SCRIPT_DIR}/instrumented"}
mkdir -p "${OUT_DIR}"
OUTPUT=${OUT:-"${OUT_DIR}/${BASENAME}.inst"}
INSTRUMENT_LIBS=${INSTRUMENT_LIBS:-1}

CSV="${SEL_INSTR:=0}"
if [ "${SEL_INSTR}" != "0" ]; then
    CSV=$(readlink -f "$CSV")
    cp "$CSV" "${E9_DIR}"
    CSV=$(basename "$CSV" .csv)
fi
echo "CSV is ${CSV}"
echo "SEL_INSTR is ${SEL_INSTR}"

shift
EXTRA_ARGS=("$@")

pushd "${E9_DIR}" >/dev/null
CC=gcc CXX=g++ ./e9compile.sh examples/schedule_memops.c
popd >/dev/null

is_shared_object() {
    file "$1" | grep -q "shared object"
}

extra_for_binary() {
    if is_shared_object "$1"; then
        return
    fi
    printf '%s\n' "--option"
    printf '%s\n' "--mem-lb=0x300000"
}

instrument_one() {
    local input=$1
    local output=$2
    shift 2

    mkdir -p "$(dirname "$output")"
    local extra=()
    while IFS= read -r item; do
        extra+=("$item")
    done < <(extra_for_binary "$input")

    echo "Instrumenting ${input} -> ${output}"
    pushd "${E9_DIR}" >/dev/null
    if [ "$SEL_INSTR" -eq 0 ]; then
        "./e9tool" \
            -o "$output" \
            -E '".plt"' -E '".plt.got"' -O2 --option --mem-granularity=4096 \
            -M 'bytes[0] == 0xF0 && mem[0].access == rw && mem[0].base != %rsp && mem[0].seg == nil' \
            -P 'mem_wri((static)addr, &mem[0], mem[0].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[1].access == rw && mem[1].base != %rsp && mem[1].seg == nil' \
            -P 'mem_wri((static)addr, &mem[1], mem[1].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[0].access == r && mem[0].base != %rsp && mem[0].seg == nil' \
            -P 'mem_ri((static)addr, &mem[0], mem[0].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[1].access == r && mem[1].base != %rsp && mem[1].seg == nil' \
            -P 'mem_ri((static)addr, &mem[1], mem[1].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[0].access == w && mem[0].base != %rsp && mem[0].seg == nil' \
            -P 'mem_wi((static)addr, &mem[0], mem[0].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[1].access == w && mem[1].base != %rsp && mem[1].seg == nil' \
            -P 'mem_wi((static)addr, &mem[1], mem[1].size)@schedule_memops' \
            --option --log=false "${extra[@]}" "${EXTRA_ARGS[@]}" -- "$input"
    else
        "./e9tool" \
            -o "$output" \
            -E '".plt"' -E '".plt.got"' -O2 --option --mem-granularity=4096 \
            --use-disasm "${CSV}.csv" \
            -M 'bytes[0] == 0xF0 && mem[0].access == rw && mem[0].base != %rsp && mem[0].seg == nil' \
            -P 'mem_wri((static)addr, &mem[0], mem[0].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[1].access == rw && mem[1].base != %rsp && mem[1].seg == nil' \
            -P 'mem_wri((static)addr, &mem[1], mem[1].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[0].access == r && mem[0].base != %rsp && mem[0].seg == nil' \
            -P 'mem_ri((static)addr, &mem[0], mem[0].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[1].access == r && mem[1].base != %rsp && mem[1].seg == nil' \
            -P 'mem_ri((static)addr, &mem[1], mem[1].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[0].access == w && mem[0].base != %rsp && mem[0].seg == nil' \
            -P 'mem_wi((static)addr, &mem[0], mem[0].size)@schedule_memops' \
            -M 'bytes[0] == 0xF0 && mem[1].access == w && mem[1].base != %rsp && mem[1].seg == nil' \
            -P 'mem_wi((static)addr, &mem[1], mem[1].size)@schedule_memops' \
            --option --log=false "${extra[@]}" "${EXTRA_ARGS[@]}" -- "$input"
    fi
    popd >/dev/null
}

discover_shared_libraries() {
    local root=$1
    local queue=("$root")
    local seen="|"
    local idx=0

    while [ "$idx" -lt "${#queue[@]}" ]; do
        local current=${queue[$idx]}
        idx=$((idx + 1))

        while IFS= read -r dep; do
            [ -n "$dep" ] || continue
            dep=$(readlink -f "$dep")
            case "$dep" in
                */ld-linux*.so*|*/ld-*.so*) continue ;;
            esac
            case "$seen" in
                *"|$dep|"*) continue ;;
            esac
            seen="${seen}${dep}|"
            printf '%s\n' "$dep"
            queue+=("$dep")
        done < <(ldd "$current" 2>/dev/null | awk '
            /=>[[:space:]]*\/[^[:space:]]+/ { print $3; next }
            /^[[:space:]]*\/[^[:space:]]+/ { print $1; next }
        ')
    done
}

instrument_one "$BINARY" "$OUTPUT"

if [ "$INSTRUMENT_LIBS" = "1" ]; then
    while IFS= read -r lib; do
        out="${OUT_DIR}/$(basename "$lib")"
        instrument_one "$lib" "$out"
    done < <(discover_shared_libraries "$BINARY")
else
    echo "Skipping linked shared libraries because INSTRUMENT_LIBS=${INSTRUMENT_LIBS}"
fi
