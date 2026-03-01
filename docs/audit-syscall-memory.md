# Performance Audit: Syscall Efficiency & Memory Layout

## Executive Summary
Top likely limiters for the 2.8M-file / 627K-dir workload are syscall volume and synchronization shape, not raw parsing speed.

1. **Per-directory `stat` is unconditionally executed** in the walker even when `--one-file-system` is off, adding ~627K avoidable metadata syscalls in your reported run.
2. **Work publication is delayed until a directory is fully read**, so very wide directories can pin one worker while others idle/steal little.
3. **All workers contend on one bounded result channel and one scan consumer**, which creates a serialization point at higher thread counts.
4. **Hot path allocation count is very high** (`RawDirEntry.name` alloc + `DirEntry.path` clone + child dir clones), creating allocator/cache pressure at multi-million-entry scale.
5. **Linux traversal is full-path based (`open_dir(None, abs_path)`)**, so each directory reopen pays path materialization + C-string conversion + full path resolution.

Conservative combined upside for default CLI behavior (disk-usage mode) is **~15–35% wall-time** if Findings 01/02/03/04/05 are implemented together. Workload-specific upside on wide trees can be much higher.

## Findings

### [FINDING-01] Unconditional per-directory `stat_entry(fd, ".")` adds avoidable syscall volume
- **Location:** `crates/hokori-walker/src/worker.rs:123-125,133-136`
- **Current behavior:** Every directory performs `stat_entry(fd, ".")` to fetch `current_dev`, then only conditionally uses it for same-filesystem pruning.
- **Issue:** The syscall is paid even when `same_filesystem == false` (default CLI path unless `-x` is set), creating one extra metadata syscall per directory.
- **Proposed fix:** Guard the dot-stat path behind `if self.same_filesystem`; when false, skip it entirely and avoid populating directory `dev` from this call.
- **Estimated impact:** Removes up to **~627K syscalls** on your reported dataset; likely **5–15%** wall-time reduction on metadata-bound runs.
- **Difficulty:** Easy

### [FINDING-02] Child work is published only after full directory read, hurting parallelism on wide directories
- **Location:** `crates/hokori-walker/src/worker.rs:138,179-181,317-324`
- **Current behavior:** Child directories are buffered in `child_dirs` and enqueued only after `read_dir_raw()` finishes and FD is closed.
- **Issue:** For shallow/wide trees (single huge directory), one worker does most parsing/stat/send work while others wait; steals happen late.
- **Proposed fix:** Publish child directory work incrementally (e.g., push every N children or immediately) to local deque/injector while scanning, with a small batch size to preserve locality.
- **Estimated impact:** Can improve wide-directory workloads by **1.5–4x**; typical mixed trees **5–20%**.
- **Difficulty:** Medium

### [FINDING-03] Single bounded result channel + single scan loop creates contention and serialization
- **Location:** `crates/hokori-walker/src/lib.rs:25-27`, `crates/hokori-walker/src/config.rs:26`, `crates/hokori-scan/src/lib.rs:132-216`
- **Current behavior:** All workers send to one bounded channel (default capacity 4096); one thread drains and processes all entries.
- **Issue:** With many workers, sender contention and backpressure increase; aggregation/tree insertion/root matching are serialized in one consumer.
- **Proposed fix:** Use sharded channels (per worker or per NUMA group) and batch-drain in scanner; optionally parallelize aggregation counters then merge.
- **Estimated impact:** **10–25%** at 8+ threads on metadata-heavy scans; reduces cache-line bouncing on channel internals.
- **Difficulty:** Hard

### [FINDING-04] Hot-path allocation count is extremely high (name + path clones)
- **Location:** `crates/hokori-sys/src/linux/getdents.rs:67-71`, `crates/hokori-sys/src/macos/getattrlistbulk.rs:225-229`, `crates/hokori-walker/src/worker.rs:160-167,180,184,239-241,293-295`, `crates/hokori-walker/src/entry.rs:4-16`
- **Current behavior:** Syscall layer allocates `RawDirEntry.name` (`to_vec()`), walker builds full path and clones it for `DirEntry`, plus extra clones for queued child dirs.
- **Issue:** Multi-million-entry scans generate millions of short-lived heap allocations, increasing allocator overhead and cache misses.
- **Proposed fix:** Move to borrowed/raw-name callback in syscall parser and delayed path materialization (e.g., parent token + name bytes) with optional arena/slab for batched ownership transfer.
- **Estimated impact:** Eliminates **millions of allocations**; likely **10–25% CPU** reduction and lower peak RSS in large scans.
- **Difficulty:** Hard

### [FINDING-05] Full-path reopen model (`open_dir(None, abs_path)`) pays repeated path-resolution costs
- **Location:** `crates/hokori-walker/src/worker.rs:100-101,115`, `crates/hokori-sys/src/linux/openat2.rs:10-16,18-27`
- **Current behavior:** Each directory is reopened by absolute path, requiring `CString` construction and full path lookup from `AT_FDCWD`; `RESOLVE_BENEATH` path-safety optimization is effectively unused.
- **Issue:** Repeated full-path resolution and string materialization add overhead at deep scale; misses dirfd-relative fast path opportunities.
- **Proposed fix:** Carry parent dirfd + child name in work items; open children via `openat2(parent_fd, name)` to avoid full path rebuilds and activate `RESOLVE_BENEATH` semantics.
- **Estimated impact:** **5–15%** on deep trees; larger security/correctness upside for symlink/path-traversal control.
- **Difficulty:** Hard

### [FINDING-06] `openat/openat2` does not attempt `O_NOATIME`
- **Location:** `crates/hokori-sys/src/linux/openat2.rs:21`, `crates/hokori-sys/src/macos/mod.rs:22`
- **Current behavior:** Directory opens use `O_RDONLY | O_CLOEXEC | O_DIRECTORY`.
- **Issue:** On mounts where atime updates are still relevant, traversal can trigger metadata writes/extra work.
- **Proposed fix:** On Linux, try `O_NOATIME` first for owned paths (or as optimistic flag), fallback on `EPERM` to current flags.
- **Estimated impact:** Usually **0–5%** (depends on mount options like `relatime/noatime`), but cheap to test.
- **Difficulty:** Easy

### [FINDING-07] Fixed 256 KiB `getdents64` buffer is not adaptive to directory-size distribution
- **Location:** `crates/hokori-sys/src/linux/getdents.rs:13-17`, `crates/hokori-walker/src/worker.rs:451-453,469`
- **Current behavior:** Each Linux worker allocates one 256 KiB read buffer regardless of actual directory fanout.
- **Issue:** If most dirs are small (<100 entries), large fixed buffers can waste cache/TLB footprint without reducing syscall count materially.
- **Proposed fix:** Use adaptive sizing (e.g., start 32–64 KiB; grow when read fills buffer repeatedly). Compare against bfs-style 64 KiB baseline.
- **Estimated impact:** **3–10%** depending on fanout distribution; also lowers per-thread memory footprint.
- **Difficulty:** Medium

### [FINDING-08] `statx` mask is not mode-aware; always requests blocks/nlink/ino
- **Location:** `crates/hokori-sys/src/linux/statx.rs:24-28`, usage in `crates/hokori-walker/src/worker.rs:276-284`, dedup use in `crates/hokori-scan/src/lib.rs:163-166`
- **Current behavior:** All statx calls request `STATX_TYPE | STATX_SIZE | STATX_BLOCKS | STATX_INO | STATX_NLINK`.
- **Issue:** For some modes (`apparent_size`, no hardlink dedup), `BLOCKS` and/or `NLINK` are unnecessary payload.
- **Proposed fix:** Thread scan intent into walker/sys layer and select minimal mask per mode (e.g., omit `STATX_BLOCKS` when only apparent size is needed; omit `STATX_NLINK` when dedup is disabled).
- **Estimated impact:** **2–8%** in stat-heavy scans; larger if filesystem has higher cost for block count retrieval.
- **Difficulty:** Medium

### [FINDING-09] TreeBuilder duplicates path bytes across structures
- **Location:** `crates/hokori-scan/src/tree.rs:46,62,72,76,82,93,136`
- **Current behavior:** Each insert allocates basename (`name`) and a full-path key (`path.to_vec()`) in `path_to_idx`; root paths are cloned multiple times during build.
- **Issue:** Build-tree mode amplifies allocation and memory usage on multi-million-node scans.
- **Proposed fix:** Intern full paths once (arena/bytes pool), use borrowed keys (`raw_entry`) or index-based parent linkage to avoid full-path key copies; store `name` as `Box<[u8]>` for tighter node footprint.
- **Estimated impact:** Saves **100MB+** in large tree builds and improves cache behavior in build phase.
- **Difficulty:** Hard

### [FINDING-10] Tree accumulation uses O(n log n) sort despite small-depth key space
- **Location:** `crates/hokori-scan/src/tree.rs:116-131`
- **Current behavior:** Builds full index vector and `sort_unstable_by(depth desc)` before parent accumulation.
- **Issue:** For millions of nodes and `u16` depth, comparison sort does extra work; also processes all nodes regardless of parent/orphan status.
- **Proposed fix:** Replace sort with depth buckets/counting sort (`Vec<Vec<NodeIdx>>` or prefix sums over max depth), and accumulate only nodes with `parent != NONE`.
- **Estimated impact:** **15–30% faster tree-build phase** on large trees; no effect when `build_tree=false`.
- **Difficulty:** Medium

### [FINDING-11] Dedup map has non-trivial memory overhead at million-hardlink scale
- **Location:** `crates/hokori-scan/src/dedup.rs:12-16,20-33`
- **Current behavior:** 128 `Mutex<AHashSet<(u64,u64)>>` shards, default growth behavior, no pre-sizing from expected hardlink cardinality.
- **Issue:** For 1M deduped entries, memory overhead is substantial. In a local measurement harness matching this shape, insertion of 1M pairs increased RSS by ~35 MB.
- **Proposed fix:** Add optional pre-size hook from scan estimates, tune shard count dynamically to thread count, and consider compact hash table mode for high-cardinality hardlink workloads.
- **Estimated impact:** **10–30 MB** memory reduction at 1M deduped entries; mild CPU gains from fewer rehashes.
- **Difficulty:** Medium

### [FINDING-12] Progress path formatting still allocates on update ticks
- **Location:** `crates/hokori-scan/src/lib.rs:140-144`, `crates/hokori-scan/src/progress.rs:61-64`
- **Current behavior:** On each throttle window, path is converted via `to_string_lossy().into_owned()` and stored as `String`.
- **Issue:** Small but avoidable allocation/UTF-8 conversion overhead in scan loop.
- **Proposed fix:** Pass borrowed bytes or `Cow<str>` through progress channel; defer lossy conversion to UI thread only when rendering.
- **Estimated impact:** Usually **<1–2%**; mostly cleanliness and reduced allocator churn.
- **Difficulty:** Easy

## Notes on requested checks
- **macOS double-stat check:** Current code already avoids regular-file double stat when bulk metadata is present (`has_bulk_meta` path) (`crates/hokori-walker/src/worker.rs:265-276` with macOS producer fields at `crates/hokori-sys/src/macos/getattrlistbulk.rs:231-234`).
- **Current struct sizes (measured):** `DirEntry = 88B`, `TreeNode = 72B`, `RawDirEntry = 96B` on this toolchain/target.
