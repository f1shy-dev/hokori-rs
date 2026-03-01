# Performance Audit: Parallelism & I/O Scheduling

**Codebase:** hokori-rs (commit: main)
**Workload:** 2.8M files, 627K dirs, 1M deduped hardlinks
**Observed:** 322s wall time (8.8K files/sec); small-test 73K files at 610K files/sec
**Throughput drop:** ~70x from cached → cold workload

---

## Executive Summary

The 70x throughput collapse at scale is driven by a combination of I/O amplification, single-threaded bottlenecks, and allocation pressure — not any single root cause. The top 5 highest-impact findings:

1. **Redundant `stat(".")` per directory** (~627K extra syscalls): Every directory does a `fstatat(fd, ".")` to get `dev` for the same-filesystem check. With `openat2(RESOLVE_NO_XDEV)` and root-dev propagation, this entire syscall is eliminable. At cold-cache scale, each stat can cost 50–200μs on HDD, totaling 30–125s of the 322s wall time.

2. **Single-threaded scan loop serialization**: All N walker threads funnel entries through one `crossbeam_channel::bounded(4096)` to a single consumer thread. At 2.8M entries, even at 300ns/entry of channel + aggregation overhead, this is ~840ms — but the real cost is backpressure stalling walker threads when the consumer can't keep up with tree-building.

3. **Per-entry heap allocations in the hot path**: Each of the ~3.4M entries allocates a `Vec<u8>` for the name in `RawDirEntry`, clones the full path into `DirEntry`, and may clone again for child directory tracking. At ~3 allocations/entry × 3.4M entries = ~10M allocations in the walk phase alone.

4. **No I/O batching (io_uring)**: Every directory open, stat, and getdents is a synchronous blocking syscall. io_uring could batch `openat` + `statx` submissions, but the current architecture (blocking threads + work-stealing) would need significant restructuring to benefit.

5. **TreeBuilder memory amplification**: The `HashMap<Vec<u8>, NodeIdx>` in TreeBuilder clones every entry's full path as a key. At 3.4M entries with average path length ~80 bytes, that's ~270MB of path keys alone, plus HashMap overhead — causing memory pressure and poor cache behavior at scale.

---

## Scalability Analysis

### Why throughput drops 70x from 73K → 2.8M entries

The 610K files/sec at 73K files represents a **fully cached** workload where all directory metadata resides in the kernel's dcache/icache. The 8.8K files/sec at 2.8M files represents a **cold-cache** workload where the kernel must read from disk. The 70x gap is explained by multiple compounding factors:

**Factor 1: Filesystem cache exhaustion (estimated 40–60x of the drop)**
- At 73K files, the entire directory tree metadata fits in kernel dcache+icache (~100–200MB).
- At 2.8M files with 627K dirs, metadata is ~2–4GB. On systems with <8GB RAM or competing workloads, this exceeds cache capacity.
- Each cold `getdents64` call on a directory not in page cache requires physical disk I/O: ~4ms on HDD per seek, ~50μs on NVMe.
- With 627K directories and sequential access, HDD seek time alone: 627K × 4ms = ~2,500s (serial). Parallelism helps but disk bandwidth is the ceiling.

**Factor 2: stat(".") amplification (estimated 1.5–2x)**
- 627K additional `fstatat(fd, ".")` syscalls. In cache: ~200ns each (negligible). Cold: each may trigger an inode read from disk.
- On ext4, the directory inode and "." are the same inode, so this is usually free if the directory itself was just opened. But the syscall overhead + kernel path resolution still adds ~300ns/call even cached.
- Net: ~190ms cached, potentially 10–60s cold (depends on inode locality).

**Factor 3: Memory pressure from TreeBuilder (estimated 2–5x at scale)**
- `TreeBuilder::insert` does `path.to_vec()` to create a HashMap key for every entry.
- At 3.4M entries × ~80 byte average path = ~270MB just for HashMap keys.
- Plus HashMap's internal table (~50 bytes/entry overhead) = ~170MB.
- Total TreeBuilder memory: ~440MB, causing L3 cache thrashing and TLB misses.
- **How to test:** Run with `--build-tree` vs without and compare walk times.

**Factor 4: Hardlink dedup at 1M entries (estimated 1.1–1.3x)**
- With 1M deduped hardlinks, at least 1M entries have `nlink > 1` and go through `dedup.check_and_insert()`.
- The dedup is single-threaded (called from the scan loop), so there's no lock contention. But 1M `AHashSet` lookups + inserts with `(u64, u64)` keys is ~15–30ms.
- The real cost: the `AHashSet` across 128 shards with ~7.8K entries/shard has poor cache locality — each lookup potentially misses L1/L2.
- **How to test:** Run with `--count-links` (disables dedup) and compare.

**Factor 5: Channel backpressure (estimated 1.1–1.5x)**
- 8 walker threads → 1 scan thread through `bounded(4096)`.
- When the scan loop is slow (tree insert, dedup check, progress update), the channel fills and walkers block on `sender.send()`.
- At 3.4M entries with 4096 capacity, each full-channel episode stalls all producers until the consumer drains enough.
- With tree building: `TreeBuilder::insert` does a HashMap lookup + insert (potentially resizing) per entry, adding ~100–500ns per entry to the consumer loop.
- **How to test:** Increase `channel_capacity` to 65536 and measure. Or instrument `sender.send()` with timing.

**Factor 6: Work-stealing warm-up (minor, ~1–2s)**
- With 1 root directory, only 1 thread has work initially. Other threads spin in `find_work()` until the first directory's children are pushed.
- For a tree with fan-out 10, it takes log₁₀(N_threads) levels before all threads are busy. With 8 threads and average depth 20, this is ~1 level = milliseconds.
- But if the root has few immediate children, warm-up can take longer.

### Compounding effect
These factors don't simply add — they compound. When TreeBuilder causes L3 thrashing, the scan loop slows, causing channel backpressure, which stalls walkers, which reduces I/O parallelism, which makes cold-cache fetches serialize more. The result is a cascading slowdown that turns a 2–3x slowdown from cache misses into a 70x collapse.

---

## Findings

### [FINDING-01] Eliminate stat(".") per directory via openat2(RESOLVE_NO_XDEV) + root_dev propagation
- **Location:** `crates/hokori-walker/src/worker.rs:124-134`
- **Current behavior:** Every `process_directory` call does `stat_entry(fd, ".")` to get `current_dev`. This is used for two purposes: (a) the `same_filesystem` cross-device check, and (b) populating `entry.dev` for child entries via `raw.dev.unwrap_or(current_dev)`.
- **Issue:** On Linux, `getdents64` returns no `dev` field, so `raw.dev` is always `None` and every entry inherits `current_dev`. This means 627K `fstatat` syscalls are issued purely to populate a field that could be derived from the parent. At cold cache, each fstatat can cost 50–200μs on HDD, totaling 30–125s. Even cached, this is 627K × ~300ns = ~190ms of pure syscall overhead.
- **Proposed fix:**
  1. When `same_filesystem` is true, propagate `root_dev` through `WorkItem` (already done) and use it directly as `current_dev`, skipping stat entirely.
  2. When `same_filesystem` is false, use `openat2` with `RESOLVE_NO_XDEV` flag to make the kernel reject cross-device opens. If `openat2` returns `EXDEV`, skip the directory. This eliminates stat while still detecting mount boundaries.
  3. For the `dev` field on entries: when `same_filesystem` is true, all entries are on the same device — use `root_dev`. When false and `openat2` succeeds, the directory is on the same device as parent, so propagate parent's dev.
  4. Only fall back to stat(".") when `same_filesystem` is false AND we need the actual `dev` value for entries on different filesystems.
- **Estimated impact:** 5–15% throughput improvement cached, 10–40% improvement cold (eliminates 627K syscalls)
- **Difficulty:** Easy
- **Dependencies:** None

### [FINDING-02] Batch channel sends — send Vec<DirEntry> chunks instead of individual entries
- **Location:** `crates/hokori-walker/src/worker.rs:175`, `crates/hokori-walker/src/lib.rs:26`
- **Current behavior:** Each entry is sent individually through `sender.send(Ok(entry))`. With ~3.4M entries across 8 threads, this is ~3.4M channel send/recv pairs. `crossbeam_channel::bounded` uses atomic CAS operations per send/recv.
- **Issue:** Channel operations have ~30–60ns overhead per send+recv (atomic CAS, cache-line bouncing between producer/consumer cores). At 3.4M entries: ~100–200ms of pure channel overhead. More importantly, each `send()` can block if the channel is full, causing fine-grained producer stalls rather than smooth batched handoffs.
- **Proposed fix:**
  1. In `process_directory`, collect entries into a local `Vec<DirEntry>` (already partially done with `child_dirs`).
  2. After processing all entries in a directory, send the entire batch as `Vec<DirEntry>` through the channel.
  3. Change channel type from `bounded<Result<DirEntry, WalkError>>` to `bounded<Vec<Result<DirEntry, WalkError>>>`.
  4. In the scan loop, drain each batch with a simple for loop.
  5. Directory average size is 2.8M/627K ≈ 4.5 entries. So batches are small. For large directories (1000+ entries), consider sending sub-batches of 256.
- **Estimated impact:** 3–8% throughput improvement (reduces channel overhead from ~200ms to ~5ms, reduces backpressure stalls)
- **Difficulty:** Easy
- **Dependencies:** None

### [FINDING-03] Eliminate per-entry name allocation in getdents64 parsing
- **Location:** `crates/hokori-sys/src/linux/getdents.rs:70`
- **Current behavior:** `parse_getdents_buf` creates `RawDirEntry { name: name.to_vec(), ... }` for every entry, allocating a new `Vec<u8>` per entry name.
- **Issue:** At 3.4M entries with average name length ~15 bytes, this is 3.4M heap allocations (~51MB). Each allocation hits the global allocator (malloc), which at high thread counts causes lock contention in glibc's arena allocator or fragmentation in jemalloc. The allocated names are short-lived — they're consumed in the callback and the underlying bytes are already in the getdents buffer.
- **Proposed fix:**
  1. Change `RawDirEntry.name` from `Vec<u8>` to a borrowed `&[u8]` tied to the buffer lifetime. This requires making `RawDirEntry` generic over a lifetime or using a callback that receives `&RawDirEntry<'_>`.
  2. The callback in `worker.rs` already copies the name into `entry_path_buf`, so it doesn't need ownership.
  3. Alternative (less invasive): pass `(&[u8], FileType, u64)` tuple directly to the callback instead of constructing `RawDirEntry`.
  4. The comment on line 68–69 acknowledges this: "RawDirEntry keeps owning Vec<u8> names to avoid threading short-lived borrowed lifetimes through public callback APIs across crates." The cross-crate API constraint is real but solvable with a lifetime parameter on the callback.
- **Estimated impact:** 5–10% throughput improvement (eliminates 3.4M small allocations, reduces allocator pressure)
- **Difficulty:** Medium (API change across crate boundary)
- **Dependencies:** None

### [FINDING-04] Reduce path cloning in the walker hot loop
- **Location:** `crates/hokori-walker/src/worker.rs:165-180`, `crates/hokori-walker/src/worker.rs:239-260`
- **Current behavior:** For every directory entry: `entry_path_buf.clone()` creates a new `Vec<u8>` for the `DirEntry` path. For child directories, an additional `entry_path_buf.clone()` creates the path for `child_dirs`. For symlinks that are followed, yet another clone occurs.
- **Issue:** Each directory entry triggers 1–2 `Vec<u8>` clones of the full path. With average path ~80 bytes and 3.4M entries, this is ~3.4M allocations for DirEntry paths + ~627K for child_dirs = ~4M allocations totaling ~320MB. These dominate the walker's allocation profile.
- **Proposed fix:**
  1. **For DirEntry paths:** Since `entry_path_buf` is a reusable buffer that's truncated and extended per entry, the clone is necessary for ownership transfer. But if batch sends are implemented (FINDING-02), entries could reference a shared arena or string interner within the batch.
  2. **For child_dirs:** Instead of storing full paths, store only the child name bytes and reconstruct the path when pushing to the deque. Child names are already in the getdents buffer (via `raw.name`) — store an offset+length into a per-directory name buffer.
  3. **Alternative:** Use a bump allocator (bumpalo) per-thread for path allocations. Reset after each directory. Entries that need to outlive the directory (sent through channel) get promoted to a shared arena.
  4. **Quick win:** Pre-compute `entry_path_buf` capacity as `parent_path.len() + 1 + MAX_NAME_LEN` (255 on Linux) to avoid reallocation during the loop.
- **Estimated impact:** 5–12% throughput improvement (eliminates ~4M allocations, reduces allocator pressure and cache misses)
- **Difficulty:** Medium
- **Dependencies:** FINDING-02 (batch sends make arena approach viable)

### [FINDING-05] CString allocation per directory — reuse a thread-local buffer
- **Location:** `crates/hokori-walker/src/worker.rs:100-113`
- **Current behavior:** `CString::new(item.path.clone())` is called at the start of `process_directory`, allocating a new `Vec<u8>` and scanning it for NUL bytes. This happens for every directory (627K times).
- **Issue:** Each call allocates `item.path.len() + 1` bytes, copies the path, appends a NUL, then scans the entire path for interior NULs. The allocation is freed when `path_cstr` is dropped at the end of the function. With 627K directories and average path ~60 bytes, this is 627K allocations totaling ~38MB.
- **Proposed fix:**
  1. Add a `path_cstr_buf: Vec<u8>` field to `WalkerWorker`, similar to the existing `buf` field.
  2. In `process_directory`, clear + extend + push NUL into the buffer, then use `CStr::from_bytes_with_nul()`.
  3. Skip the NUL scan — paths from `WorkItem` are constructed by the walker itself from getdents names (which can't contain NUL on Linux/macOS).
  4. Already partially done for entry names with `name_to_cstr` and `name_c_buf` — apply the same pattern to directory paths.
- **Estimated impact:** 1–3% throughput improvement (eliminates 627K allocations)
- **Difficulty:** Easy
- **Dependencies:** None

### [FINDING-06] TreeBuilder HashMap path key duplication — use arena-backed keys or path interning
- **Location:** `crates/hokori-scan/src/tree.rs:62`
- **Current behavior:** `TreeBuilder::insert` calls `path.to_vec()` to create an owned `Vec<u8>` key for `path_to_idx: HashMap<Vec<u8>, NodeIdx>`. Every entry's full path is duplicated — once in the `TreeNode.name` (as basename) and once as a full-path HashMap key.
- **Issue:** At 3.4M entries with average path ~80 bytes, HashMap keys alone consume ~270MB. Plus HashMap's internal overhead (bucket array, metadata bytes) at ~50 bytes/entry = ~170MB. Total: ~440MB. This is a major source of memory pressure at scale, causing:
  - L3 cache thrashing (440MB >> typical 16–32MB L3)
  - TLB misses (440MB / 4KB pages = ~110K pages)
  - Allocator fragmentation from millions of variable-size allocations
  - The `build()` phase iterates all entries to link parents, doing HashMap lookups with `parent_path()` — each lookup hashes ~60 bytes and traverses buckets, thrashing cache lines.
- **Proposed fix:**
  1. **Phase 1 (Medium):** Replace `HashMap<Vec<u8>, NodeIdx>` with `hashbrown::HashMap` using raw entries and an arena allocator for keys. All paths are allocated from a contiguous arena, improving cache locality.
  2. **Phase 2 (Hard):** Eliminate the HashMap entirely. Since entries arrive in DFS order (due to LIFO work-stealing), maintain a stack of `(depth, NodeIdx)` pairs. When an entry arrives at depth D, pop the stack until the top has depth < D — that's the parent. This is O(1) amortized and uses O(max_depth) memory instead of O(N).
  3. **Phase 3 (Hard):** Use path component interning. Each unique path component (directory name) gets interned once. Paths are stored as sequences of intern IDs. This deduplicates the shared prefixes — `/home/user/projects/foo/src` and `/home/user/projects/foo/lib` share 4 of 5 components.
- **Estimated impact:** 10–25% throughput improvement for tree-building workloads (eliminates ~440MB of HashMap overhead, dramatically improves cache behavior)
- **Difficulty:** Medium (Phase 1), Hard (Phase 2/3)
- **Dependencies:** None

### [FINDING-07] Inode-order traversal for cold-cache HDD workloads
- **Location:** `crates/hokori-sys/src/linux/getdents.rs:46-81` (parse loop), `crates/hokori-walker/src/worker.rs:154-308` (callback processing)
- **Current behavior:** Entries from `getdents64` are processed in the order the kernel returns them (hash-tree order on ext4, B-tree order on XFS). The worker processes and stats each entry in this order.
- **Issue:** On ext4, directory entries are stored in an HTree (hash-indexed), so `getdents64` returns entries in hash order — essentially random relative to inode number. When the inode table is laid out sequentially on disk (which it is for most filesystems), processing entries in hash order causes random seeks across the inode table. For cold-cache HDD workloads, each seek costs ~4ms. With ~4.5 entries per directory (average), the seeks within a directory are small, but across directories the effect compounds.
- **Proposed fix:**
  1. After `getdents64` returns all entries for a directory, sort them by inode number (`d_ino`) before processing.
  2. This requires buffering all entries from one directory before the callback fires. Change `read_dir_raw` to collect entries into a `Vec` first, sort by inode, then invoke callbacks.
  3. On NVMe/SSD, this is neutral (random access is fast). On HDD, this can be a significant win — GNU `find` and `bfs` both do this.
  4. Make it configurable or auto-detect via `/sys/block/*/queue/rotational`.
- **Estimated impact:** 20–50% throughput improvement on HDD, neutral on SSD/NVMe
- **Difficulty:** Easy
- **Dependencies:** None (but changes `read_dir_raw` API slightly)

### [FINDING-08] Work-stealing warm-up is slow for single-root scans
- **Location:** `crates/hokori-walker/src/worker.rs:369-435`
- **Current behavior:** All root paths are pushed to the global `Injector`. With 1 root (common case), only 1 `WorkItem` exists initially. The first thread to steal it processes the root directory and pushes its children to its local deque. Other threads must wait until children are available to steal.
- **Issue:** For a tree with root fan-out F, it takes ceil(log_F(N_threads)) directory levels before all threads have work. With 8 threads and typical fan-out of 10–50, this is 1 level (fast). But with fan-out 2 (binary tree structure, common in deep paths), it takes 3 levels. Each level requires the parent to be fully processed (openat + stat + getdents) before children are available. At ~100μs per directory (cold), warm-up can take 300μs for binary trees.
  
  The bigger issue: during warm-up, idle threads sleep with exponential backoff starting at 50μs. After 1 miss, they sleep 50μs; after 2, 100μs; after 3, 200μs. A thread that misses 3 times sleeps 350μs total before checking again. If the first thread pushes children in 100μs, threads that started backing off have already slept past the availability.
- **Proposed fix:**
  1. **Pre-explode roots:** Before spawning walker threads, do a single-threaded pre-scan of the root directory and push all its children directly into the injector. This gives N work items immediately, one per thread.
  2. **Reduce initial backoff:** Start at 10μs instead of 50μs, or use `thread::yield_now()` for the first few iterations before sleeping.
  3. **Use crossbeam-utils `Backoff`:** The `crossbeam_utils::Backoff` type implements spin-then-yield-then-park, which is more responsive than pure sleep for short waits.
- **Estimated impact:** 2–5% throughput improvement for small-to-medium scans (mostly amortized for large scans)
- **Difficulty:** Easy
- **Dependencies:** None

### [FINDING-09] Linux: Use AT_EMPTY_PATH for fstat instead of fstatat(fd, ".")
- **Location:** `crates/hokori-walker/src/worker.rs:124`, `crates/hokori-sys/src/linux/statx.rs:21-56`
- **Current behavior:** `stat_entry(fd, ".")` calls `statx(fd, ".", flags, mask, &stx)`. The kernel resolves "." by looking up the current directory entry from the fd's dentry — but it still has to do a pathname lookup through the VFS layer.
- **Issue:** Using `statx(fd, "", AT_EMPTY_PATH | other_flags, mask, &stx)` skips the pathname lookup entirely. The kernel goes directly from `fd → inode → stat`. This saves ~50–100ns per call by avoiding VFS path resolution overhead. At 627K directories, this saves ~30–60ms.
- **Proposed fix:**
  1. Change `stat_entry` to accept an `AT_EMPTY_PATH` mode where `name` is `""` and the flag `AT_EMPTY_PATH` is added.
  2. Create a `stat_fd(fd: i32)` convenience function that calls `statx(fd, "", AT_EMPTY_PATH, ...)`.
  3. Use `stat_fd` for the same-filesystem check in `process_directory`.
  4. Note: `AT_EMPTY_PATH` requires Linux 5.8+ for statx (older kernels may return EINVAL). Add a runtime fallback.
- **Estimated impact:** 0.5–1% throughput improvement (saves ~50ms at 627K dirs)
- **Difficulty:** Easy
- **Dependencies:** Partially redundant with FINDING-01 (if stat(".") is eliminated entirely)

### [FINDING-10] Single-threaded scan loop limits consumer throughput
- **Location:** `crates/hokori-scan/src/lib.rs:132-219`
- **Current behavior:** The `for result in entry_rx` loop processes every entry sequentially: dedup check → size computation → aggregation → tree insert → progress update → root matching.
- **Issue:** The per-entry cost breakdown in the scan loop:
  - Channel recv: ~30ns
  - Root matching (`path_has_prefix` linear scan): ~10ns with 1 root
  - Size computation: ~5ns
  - Dedup check (nlink > 1 path): ~50–100ns (hash + set lookup)
  - Aggregation: ~5ns (arithmetic)
  - Tree insert: ~200–500ns (HashMap lookup + insert + path.to_vec())
  - Progress check: ~5ns (timestamp compare, amortized)
  - Total per entry: ~300–700ns → at 3.4M entries = 1.0–2.4s

  Without tree building, it's ~100–200ns/entry = ~340–680ms. This is fast enough to not bottleneck the walker. But WITH tree building, the scan loop at ~500ns/entry (1.7s total) can cause backpressure on the channel when walkers are fast (cached workload), throttling their I/O throughput.
- **Proposed fix:**
  1. **Batch drain:** Use `entry_rx.try_recv()` in a loop to drain up to 256 entries into a local `Vec`, then process the batch. This amortizes the channel overhead and improves CPU cache locality (process entries while they're in L1).
  2. **Separate tree building:** Move tree insertion to a second thread. The scan loop sends `(path_bytes, size, is_dir, depth)` tuples to a tree-builder thread via a separate channel. This parallelizes aggregation and tree building.
  3. **Parallel aggregation (future):** Shard the aggregator by root index. Each walker thread maintains per-root counters. Merge at the end. This eliminates the scan loop for aggregation entirely.
- **Estimated impact:** 3–8% throughput improvement with tree building (eliminates scan loop as bottleneck on cached workloads)
- **Difficulty:** Medium
- **Dependencies:** FINDING-02 (batch sends complement batch drain)

### [FINDING-11] macOS: getattrlistbulk returns metadata but directories still lack nlink/size
- **Location:** `crates/hokori-sys/src/macos/getattrlistbulk.rs:230-234`, `crates/hokori-walker/src/worker.rs:264-306`
- **Current behavior:** On macOS, `getattrlistbulk` returns `ATTR_FILE_LINKCOUNT`, `ATTR_FILE_ALLOCSIZE`, and `ATTR_FILE_DATALENGTH` — but these are **file** attributes, only populated for regular files (`VREG`). For directories, `size`, `alloc_size`, and `nlink` in `RawDirEntry` are `None`. The worker's catch-all arm (line 264) checks `has_bulk_meta` — for directories on macOS, this is false, triggering a fallback `stat_entry` call.
  
  Wait — actually, directories go through the `FileType::Directory` arm (line 164), which doesn't call stat. It uses `raw.size` (None) and `raw.alloc_size.or(raw.size)` (None). So directories on macOS get `apparent_size: None` and `disk_usage: None`. This is correct behavior — directory size isn't meaningful for disk usage reporting.
  
  However, for `FileType::Other` entries (pipes, sockets, block devices), the catch-all arm WILL attempt a stat call because `has_bulk_meta` is false for non-files.
- **Issue:** On macOS, non-file/non-directory/non-symlink entries (rare: block devices, char devices, sockets, FIFOs) trigger an unnecessary `fstatat` call even though their size is typically 0 or irrelevant. This is a micro-optimization — these entries are rare.
  
  More importantly: macOS `getattrlistbulk` requests `ATTR_FILE_LINKCOUNT` but this only works for files. To get `nlink` for directories (needed for hardlink detection if directories could be hardlinked — they can't on macOS), a separate `ATTR_DIR_LINKCOUNT` would be needed, or use `ATTR_CMN_OBJPERMANENTID` for dedup.
- **Proposed fix:**
  1. For the catch-all arm, skip stat for entries where size isn't needed (e.g., `FileType::Other` with `apparent_size: Some(0)`).
  2. Add `ATTR_CMN_DEVID` to the getattrlistbulk request (already done — line 42) to ensure `dev` is always populated, avoiding stat for the device field.
  3. Consider requesting `ATTR_DIR_LINKCOUNT` alongside `ATTR_FILE_LINKCOUNT` to get nlink for directories too.
- **Estimated impact:** <1% throughput improvement (only affects rare FileType::Other entries)
- **Difficulty:** Easy
- **Dependencies:** None

### [FINDING-12] Thread count default doesn't account for I/O-bound nature of filesystem scanning
- **Location:** `crates/hokori-walker/src/config.rs:39-51`
- **Current behavior:** `resolved_threads()` uses `available_parallelism()` capped at 32. On a 16-core machine, this spawns 16 threads.
- **Issue:** Filesystem scanning is I/O-bound, not CPU-bound. Each thread spends most of its time blocked in `openat` + `getdents64` + `statx` syscalls. During a syscall, the thread's CPU core is idle (for synchronous I/O). Optimal thread count for I/O-bound work is typically 2–4x the CPU count, because:
  - While thread A is blocked in a syscall, thread B can process its results
  - More threads = more concurrent I/O requests = better utilization of NVMe's internal parallelism (NVMe SSDs can handle 64K+ concurrent I/O ops)
  - The kernel's I/O scheduler can batch and reorder more requests with more in-flight operations
  
  However, too many threads causes: context switch overhead, deque contention, memory pressure (each thread has a 256KB getdents buffer).
  
  For HDD: 2–4 threads is optimal (more threads cause seek storms). For NVMe: 32–64 threads may be optimal. The current cap of 32 is conservative for NVMe and aggressive for HDD.
- **Proposed fix:**
  1. Auto-detect storage type via `/sys/block/<dev>/queue/rotational` on Linux (0 = SSD/NVMe, 1 = HDD).
  2. For HDD: default to `min(4, available_parallelism())`.
  3. For SSD/NVMe: default to `min(available_parallelism() * 2, 64)`.
  4. Document the `--threads` flag with guidance: "Default auto-detects. For NVMe, try 32–64. For HDD, try 2–4."
- **Estimated impact:** 10–30% throughput improvement on NVMe (more I/O parallelism), 20–50% improvement on HDD (less seek contention)
- **Difficulty:** Medium (auto-detection is platform-specific)
- **Dependencies:** None

### [FINDING-13] Atomic ordering on cancel check is per-entry but could be per-directory
- **Location:** `crates/hokori-walker/src/worker.rs:156`, `crates/hokori-walker/src/worker.rs:42`
- **Current behavior:** `cancel.load(Ordering::Relaxed)` is checked inside the `read_dir_raw` callback (per-entry) and at the top of the `run()` loop (per-directory).
- **Issue:** The per-entry check adds ~1–2ns per entry (Relaxed load is essentially free on x86 — just a regular memory read — but on ARM it may involve a load-acquire). At 3.4M entries, total overhead is ~3–7ms. This is negligible.
  
  However, the per-entry cancel check includes a `cancel.clone()` on line 143, which clones the `Arc<AtomicBool>`. This doesn't clone the bool — it increments the Arc's reference count (atomic increment). Wait — `cancel` is cloned ONCE before the `read_dir_raw` call, not per-entry. So the per-entry cost is truly just the Relaxed load. This is fine.
- **Proposed fix:** No change needed. The per-entry check enables responsive cancellation (useful for interactive TUI). The overhead is negligible. If anything, the `Arc::clone` of `sender` and `cancel` per-directory (lines 142–143) could be avoided by passing references, but the closure needs `'static` for the callback, so this is a fundamental constraint.
- **Estimated impact:** <0.1% (not worth changing)
- **Difficulty:** N/A
- **Dependencies:** N/A

### [FINDING-14] root_path_bytes linear scan per entry
- **Location:** `crates/hokori-scan/src/lib.rs:147-148`
- **Current behavior:** `root_path_bytes.iter().position(|root| path_has_prefix(entry.path_bytes(), root))` does a linear scan over all roots for every entry to determine which root an entry belongs to.
- **Issue:** With 1 root (common case), this is O(1) — a single `path_has_prefix` check (~10ns). With N roots, it's O(N) per entry. For 10 roots × 3.4M entries = 34M prefix checks. Each `path_has_prefix` does `starts_with` on byte slices, which is fast but still ~10ns × 34M = ~340ms.
  
  More importantly, the root matching could be eliminated entirely by having the walker tag each entry with its root index. The walker already knows which root each `WorkItem` originated from.
- **Proposed fix:**
  1. Add a `root_idx: u16` field to `WorkItem` and `DirEntry`.
  2. Set `root_idx` when seeding roots in `spawn_walk` (line 429: `root_idx: i as u16`).
  3. Propagate through child_dirs: children inherit their parent's `root_idx`.
  4. In the scan loop, use `entry.root_idx` directly instead of searching.
- **Estimated impact:** <1% with 1 root, 5–10% with many roots
- **Difficulty:** Easy
- **Dependencies:** None

### [FINDING-15] getdents buffer size vs directory size distribution
- **Location:** `crates/hokori-sys/src/linux/getdents.rs:17`
- **Current behavior:** `GETDENTS_BUF_SIZE = 256KB`. Each thread allocates one buffer. At 32 threads = 8MB total.
- **Issue:** Most directories are small (< 100 entries, fitting in ~4KB of getdents output). A 256KB buffer means 252KB is wasted per call for small directories. However, the buffer is reused across directories, so the waste is only memory, not allocation overhead. The real question is whether 256KB is large enough for huge directories (100K+ entries).
  
  A directory with 100K entries × ~24 bytes/entry (d_ino + d_off + d_reclen + d_type + name) = ~2.4MB. This requires ~10 `getdents64` calls with a 256KB buffer. Each call is a syscall with ~200ns overhead. At 10 calls × 200ns = 2μs overhead — negligible.
  
  The trade-off is correct: 256KB is a good balance. Larger buffers would help only for very large directories (>10K entries), which are rare.
- **Proposed fix:** No change needed. The current 256KB is well-tuned. If targeting huge directories specifically, consider detecting large directories (via `stat.st_size` which reflects directory file size) and dynamically resizing the buffer.
- **Estimated impact:** <0.5% (not worth changing)
- **Difficulty:** N/A
- **Dependencies:** N/A

---

## io_uring Integration Roadmap

### Architecture Overview

The current architecture is **synchronous blocking threads + work-stealing**. io_uring integration requires rethinking how I/O operations are submitted and completed, because io_uring is fundamentally asynchronous — you submit operations and poll for completions.

### Key Insight: Which syscalls benefit from io_uring?

| Syscall | Frequency | io_uring support | Benefit |
|---------|-----------|-----------------|---------|
| `openat2` | 627K (1 per dir) | `IORING_OP_OPENAT2` | Medium — can batch opens |
| `getdents64` | ~700K (1.1 per dir avg) | `IORING_OP_GETDENTS` (Linux 6.6+) | High — can overlap with processing |
| `statx` | ~627K (stat ".") + rare DT_UNKNOWN | `IORING_OP_STATX` | High if stat("." ) isn't eliminated; Low otherwise |
| `close` | 627K (1 per dir) | `IORING_OP_CLOSE` | Low — close is fast |

At 2.8M files on ext4/xfs/btrfs, `DT_UNKNOWN` entries are rare (<1%). So `statx` calls are mostly from stat(".") (FINDING-01). If FINDING-01 is implemented, the remaining statx calls are minimal, and io_uring's statx batching benefit is small.

The big win from io_uring is **overlapping openat + getdents**: while one directory's getdents results are being processed, the next directory's openat can already be in-flight.

### Phase 1: io_uring for openat + close (Medium difficulty, 2–3 weeks)

**Goal:** Overlap directory opens with processing. Submit `openat2` for the next N directories while processing the current one.

**Crate choice:** Use the `io-uring` crate (low-level, no runtime dependency). Avoid `tokio-uring` (requires tokio runtime, heavyweight) and `monoio` (requires its own runtime).

**Architecture:**
```
Per-thread io_uring instance (ring size 64)
  │
  ├── Submit: IORING_OP_OPENAT2 for next 4-8 directories from local deque
  ├── Process: current directory (getdents64 synchronous, entries via callback)  
  ├── Peek: check for openat completions → fd ready for next process_directory
  └── Submit: IORING_OP_CLOSE for finished fds (fire-and-forget)
```

Each worker thread owns a private `IoUring` instance. No sharing between threads. The work-stealing deque and channel architecture remain unchanged.

**Estimated impact:** 5–15% on cold workloads (hides openat latency behind processing)

### Phase 2: io_uring for getdents64 (Hard, 4–6 weeks)

**Goal:** Batch `getdents64` calls. Read multiple directories concurrently.

**Requires:** Linux 6.6+ for `IORING_OP_GETDENTS` (added in kernel commit `f30daec`).

**Architecture:**
```
Per-thread io_uring instance (ring size 256)
  │
  ├── Take 8 directories from local deque
  ├── Submit: 8 × IORING_OP_OPENAT2
  ├── As opens complete: submit IORING_OP_GETDENTS for each fd
  ├── As getdents complete: parse buffer, send entries, submit next getdents (if more data)
  ├── When directory done: submit IORING_OP_CLOSE, push children to deque
  └── Repeat
```

This is significantly more complex because it requires managing multiple in-flight directories per thread, with state machines tracking each directory's lifecycle (opening → reading → parsing → closing).

**Estimated impact:** 15–40% on cold HDD workloads (multiple directories read concurrently reduces effective seek time)

### Phase 3: io_uring for batched statx (Medium, 1–2 weeks — only if FINDING-01 is NOT implemented)

**Goal:** Batch `statx` calls for directories that need device ID.

**Architecture:**
```
After getdents64 returns all entries for a directory:
  │
  ├── Collect entries needing stat (DT_UNKNOWN type)
  ├── Submit N × IORING_OP_STATX (N = count of DT_UNKNOWN entries)
  ├── Wait for all completions
  └── Process entries with metadata
```

On ext4/xfs/btrfs, DT_UNKNOWN is rare, so this batch is typically empty. The win is only meaningful on network filesystems (NFS, CIFS) where DT_UNKNOWN is common.

**Optimal batch size:** 32–64 per submission. At 2.8M files with 1% DT_UNKNOWN = 28K statx calls. In batches of 32 = 875 submissions. Each submission + completion poll ≈ 500ns. Total: ~440μs — negligible. The win is latency hiding, not syscall reduction.

**Estimated impact:** <5% on local filesystems, 20–50% on network filesystems

### Phase 4: Fully async walker (Hard, 8–12 weeks)

**Goal:** Replace the entire synchronous thread pool with an io_uring-native event loop.

**Architecture:**
```
1-2 io_uring submission threads (not per-CPU — I/O bound)
  │
  ├── Manage 256+ in-flight directory operations
  ├── State machine per directory: OPEN → GETDENTS → PARSE → CLOSE
  ├── Completion-driven: CQE triggers next phase
  ├── Work distribution: directories assigned round-robin to submission threads
  └── Entries sent to N consumer threads for aggregation
```

This is a fundamental architecture change. The work-stealing deque is replaced by a shared work queue. Thread count drops from N (CPU count) to 2–4 submission threads + 1–2 consumer threads.

**Crate choice:** Consider `monoio` or `glommio` for this phase, as they provide io_uring-native runtimes with good ergonomics.

**Estimated impact:** 2–5x throughput improvement on NVMe (full I/O parallelism), 1.5–3x on HDD (optimal request ordering)

### Compatibility matrix

| Phase | Min kernel | Fallback | Risk |
|-------|-----------|----------|------|
| Phase 1 | 5.6 | openat2 → openat | Low |
| Phase 2 | 6.6 | Sync getdents64 | Medium (new kernel) |
| Phase 3 | 5.6 | Sync statx | Low |
| Phase 4 | 6.6 | Full sync walker | High |

### Recommended approach

1. Implement FINDING-01 through FINDING-08 first (easy/medium wins, no io_uring needed).
2. Implement Phase 1 (io_uring openat) — this is the best risk/reward ratio.
3. Benchmark. If cold-cache throughput is still the bottleneck, proceed to Phase 2.
4. Phase 4 is a rewrite — only pursue if aiming for "fastest possible" on NVMe.

---

## Appendix: Measurement Plan

To validate these findings, run the following experiments on the 2.8M-file workload:

| Experiment | Command | Measures |
|-----------|---------|----------|
| Baseline | `hokori . --stats --timings` | Walk time, throughput |
| No tree | `hokori . --stats --timings` (without `--build-tree`) | TreeBuilder impact |
| No dedup | `hokori . --stats --timings --count-links` | Dedup overhead |
| No tree + no dedup | Combine above | Pure walker throughput |
| Thread scaling | `hokori . --stats -t 1` through `-t 64` | Optimal thread count |
| strace syscall count | `strace -c hokori .` | Syscall breakdown |
| perf cache misses | `perf stat -e cache-misses,cache-references hokori .` | Cache pressure |
| perf flamegraph | `cargo flamegraph -- .` | Hot function identification |

For the 73K cached workload, repeat all experiments to isolate CPU-bound bottlenecks from I/O-bound ones.