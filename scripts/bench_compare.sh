#!/usr/bin/env bash
set -euo pipefail

TARGET="${1:-$HOME}"
RUNS="${BENCH_RUNS:-5}"
WARMUP="${BENCH_WARMUP:-2}"

echo "=== hokori-rs benchmark comparison ==="
echo "Target: $TARGET"
echo "Runs: $RUNS (warmup: $WARMUP)"
echo ""

TOOLS=("hokori")
command -v du >/dev/null && TOOLS+=("du")
command -v diskus >/dev/null && TOOLS+=("diskus")
command -v dust >/dev/null && TOOLS+=("dust")
command -v dua >/dev/null && TOOLS+=("dua")
command -v gdu >/dev/null && TOOLS+=("gdu")

echo "Available tools: ${TOOLS[*]}"
echo ""

bench() {
    local name="$1"
    shift
    local cmd=("$@")

    for ((i=0; i<WARMUP; i++)); do
        "${cmd[@]}" >/dev/null 2>&1 || true
    done

    local times=()
    for ((i=0; i<RUNS; i++)); do
        local start end elapsed
        start=$(date +%s%N)
        "${cmd[@]}" >/dev/null 2>&1 || true
        end=$(date +%s%N)
        elapsed=$(((end - start) / 1000000))
        times+=("$elapsed")
    done

    local min=${times[0]} max=${times[0]} sum=0
    for t in "${times[@]}"; do
        ((sum += t))
        ((t < min)) && min=$t
        ((t > max)) && max=$t
    done
    local avg=$((sum / RUNS))

    printf "  %-12s  min: %6dms  avg: %6dms  max: %6dms\n" "$name" "$min" "$avg" "$max"
}

echo "--- Cold cache (drop_caches if root, else just first run) ---"
if [ "$(id -u)" = "0" ]; then
    sync && echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
fi

for tool in "${TOOLS[@]}"; do
    case "$tool" in
        hokori)  bench "hokori" cargo run --release -p hokori-cli -- -q "$TARGET" ;;
        du)      bench "du" du -sb "$TARGET" ;;
        diskus)  bench "diskus" diskus "$TARGET" ;;
        dust)    bench "dust" dust -n 0 "$TARGET" ;;
        dua)     bench "dua" dua "$TARGET" ;;
        gdu)     bench "gdu" gdu -n "$TARGET" ;;
    esac
done

echo ""
echo "--- Warm cache ---"
for tool in "${TOOLS[@]}"; do
    case "$tool" in
        hokori)  bench "hokori" cargo run --release -p hokori-cli -- -q "$TARGET" ;;
        du)      bench "du" du -sb "$TARGET" ;;
        diskus)  bench "diskus" diskus "$TARGET" ;;
        dust)    bench "dust" dust -n 0 "$TARGET" ;;
        dua)     bench "dua" dua "$TARGET" ;;
        gdu)     bench "gdu" gdu -n "$TARGET" ;;
    esac
done
