#!/usr/bin/env bash
set -euo pipefail

OUTPUT="${1:-/tmp/hokori-bench-tree}"
NUM_FILES="${2:-100000}"
NUM_DIRS="${3:-5000}"
MAX_DEPTH="${4:-10}"

echo "Generating test tree: $NUM_FILES files, $NUM_DIRS dirs, depth $MAX_DEPTH"
echo "Output: $OUTPUT"

rm -rf "$OUTPUT"
mkdir -p "$OUTPUT"

dirs=("$OUTPUT")
for ((i=0; i<NUM_DIRS; i++)); do
    parent_idx=$((RANDOM % ${#dirs[@]}))
    parent="${dirs[$parent_idx]}"
    depth=$(echo "$parent" | tr '/' '\n' | wc -l)
    if ((depth < MAX_DEPTH)); then
        new_dir="$parent/d$i"
        mkdir -p "$new_dir"
        dirs+=("$new_dir")
    fi
done

echo "Created ${#dirs[@]} directories"

sizes=(0 100 1024 4096 65536 1048576)
for ((i=0; i<NUM_FILES; i++)); do
    dir_idx=$((RANDOM % ${#dirs[@]}))
    size_idx=$((RANDOM % ${#sizes[@]}))
    size=${sizes[$size_idx]}
    dd if=/dev/urandom of="${dirs[$dir_idx]}/f$i" bs=1 count="$size" 2>/dev/null

    if ((i % 10000 == 0)); then
        echo "  Created $i / $NUM_FILES files..."
    fi
done

HARDLINK_COUNT=$((NUM_FILES / 20))
for ((i=0; i<HARDLINK_COUNT; i++)); do
    src_dir_idx=$((RANDOM % ${#dirs[@]}))
    dst_dir_idx=$((RANDOM % ${#dirs[@]}))
    src="${dirs[$src_dir_idx]}/f$((RANDOM % NUM_FILES))"
    if [ -f "$src" ]; then
        ln "$src" "${dirs[$dst_dir_idx]}/hl$i" 2>/dev/null || true
    fi
done

echo "Created $HARDLINK_COUNT hardlinks"

SYMLINK_COUNT=$((NUM_FILES / 50))
for ((i=0; i<SYMLINK_COUNT; i++)); do
    src_dir_idx=$((RANDOM % ${#dirs[@]}))
    dst_dir_idx=$((RANDOM % ${#dirs[@]}))
    src="${dirs[$src_dir_idx]}/f$((RANDOM % NUM_FILES))"
    if [ -f "$src" ]; then
        ln -s "$src" "${dirs[$dst_dir_idx]}/sl$i" 2>/dev/null || true
    fi
done

echo "Created $SYMLINK_COUNT symlinks"
echo ""
du -sh "$OUTPUT"
echo "Done."
