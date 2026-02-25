# Complementary Disk Analysis/Cleanup Tools: rmlint, gdu, ncdu, squirreldisk

## Executive summary

- **rmlint has the strongest duplicate-finding pipeline** of the four: traversal → preprocess (size/inode/path dedup) → incremental hashing/shredding → output. Its architecture is built to avoid full-file reads whenever possible and to keep I/O scheduling device-aware.
- **gdu is a parallel tree scanner, not a deduper**. It emphasizes fast disk usage analysis through recursive goroutine fan-out with a global concurrency limiter (`3 * GOMAXPROCS`), progressive UI channels, and optional storage-backed analyzers (Badger/SQLite) when in-memory trees become too heavy.
- **ncdu is highly efficient via simplicity and memory layout discipline**: single-threaded, syscall-light traversal (`chdir` + `readdir` + `lstat`), compact per-node struct with flexible array names, and a streaming export format that avoids holding the entire tree when running in export mode.
- **squirreldisk’s Rust backend is orchestration-heavy, scanner-light**: it launches a sidecar (`pdu`) for actual scanning, streams stdout JSON to the frontend, parses stderr progress with regex, and emits Tauri events. Treemap preparation is effectively delegated to sidecar JSON + frontend processing.
- For a **disk cleaner roadmap**, best synthesis is: rmlint-style candidate reduction + progressive verification, gdu/ncdu-style responsive progress channels, ncdu-like compact export contracts, and squirreldisk-style process isolation for platform-specific scan engines.

---

## 1) rmlint deep dive (C): scanning pipeline, incremental hashing, reflink awareness

rmlint’s core run path is explicit in `rm_cmd_main()`:

```c
// /tmp/rmlint/lib/cmdline.c
rm_fmt_set_state(session->formats, RM_PROGRESS_STATE_TRAVERSE);
session->mds = rm_mds_new(cfg->threads, session->mounts, cfg->fake_pathindex_as_disk);
rm_traverse_tree(session);

rm_fmt_set_state(session->formats, RM_PROGRESS_STATE_PREPROCESS);
rm_preprocess(session);

if(cfg->find_duplicates || cfg->merge_directories) {
    rm_shred_run(session);
}

if(cfg->merge_directories) {
    rm_fmt_set_state(session->formats, RM_PROGRESS_STATE_MERGE);
    rm_tm_finish(session->dir_merger);
}
```

### 1.1 Pass 1: traversal architecture (parallelized, device-aware)

Traversal is implemented in `traverse.c` using `fts_*` APIs, then fed into an **MDS (multi-disk scheduler)** abstraction.

Key points:

- `rm_traverse_tree()` configures MDS with traversal worker callback and `threads_per_disk`.
- It pushes initial path tasks per root directory and then runs `rm_mds_start()`/`rm_mds_finish()`.
- Traversal worker (`rm_traverse_directory`) uses `fts_read()` and handles hidden filters, cross-device boundaries, symlink behavior, empty dir detection, and special lint types.

```c
// /tmp/rmlint/lib/traverse.c
rm_mds_configure(mds,
                 (RmMDSFunc)rm_traverse_directory,
                 trav_session,
                 0,
                 cfg->threads_per_disk,
                 NULL);
...
rm_mds_start(mds);
rm_mds_finish(mds);
```

MDS internals (`md-scheduler.c`) show a queue-per-device model and thread caps:

```c
// /tmp/rmlint/lib/md-scheduler.c
guint threads = CLAMP(mds->threads_per_disk * disk_count, 1, (guint)mds->max_threads);
mds->pool = rm_util_thread_pool_new((GFunc)rm_mds_factory, mds, threads);
```

For rotational devices, tasks can be prioritized by disk offset (`rm_mds_elevator_cmp`) to reduce seek overhead.

### 1.2 Pass 2: preprocess (size grouping + inode/path normalization)

`rm_preprocess()` performs a critical reduction phase before expensive hashing:

1. Sort all candidates by size/criteria.
2. Partition by size group.
3. Inside each group, cluster inode-equivalent files (hardlinks/path doubles).
4. Move non-duplicate lint types away from duplicate path.

```c
// /tmp/rmlint/lib/preprocess.c
/* After rm_preprocess(), all remaining duplicate candidates are in
 * session->tables->size_groups->group1->file1a ...
 */
g_queue_sort(all_files, (GCompareDataFunc)rm_file_cmp_full, session);
...
if(!file || rm_file_cmp_split(file, current_size_file, session) != 0) {
    tables->size_groups = g_slist_prepend(tables->size_groups, NULL);
    removed += g_hash_table_foreach_remove(node_table,
                                           (GHRFunc)rm_pp_handle_inode_clusters,
                                           session);
}
```

This is where rmlint avoids wasting hash I/O on obvious non-candidates.

### 1.3 Pass 3: shredder (incremental hashing and refinement)

`rm_shred_run()` drives duplicate verification. The design notes in `shredder.c` are excellent and explicit:

- device workers are threadpool-driven
- hashing pipes are single-thread per file stream to preserve hash order
- progressive read increments are used to avoid full reads unless needed

Increment sizing is adaptive:

```c
// /tmp/rmlint/lib/shredder.c
RmOff balanced_bytes = tag->page_size * SHRED_BALANCED_PAGES;
RmOff target_bytes = balanced_bytes * group->offset_factor;
...
if(group->hash_offset + target_bytes + balanced_bytes >= group->file_size) {
    group->next_offset = group->file_size;
} else {
    group->next_offset = group->hash_offset + target_bytes;
}
```

Then per file:

```c
// /tmp/rmlint/lib/shredder.c
RmOff bytes_to_read = rm_shred_get_read_size(file, tag);
RmHasherTask *task = rm_hasher_task_new(tag->hasher, file->digest, file);
rm_hasher_task_hash(task, file_path, file->hash_offset, bytes_to_read, file->is_symlink, &bytes_read);
file->hash_offset += bytes_to_read;
rm_hasher_task_finish(task);
```

This is **incremental progressive hashing** (start at offset 0, grow chunk size per surviving group) rather than a fixed “first 4K + last 4K + full” algorithm. Practical outcome is the same goal: prune quickly, read fully only where collision candidates persist.

### 1.4 Hash strategy and algorithms

rmlint supports a broad digest registry:

```c
// /tmp/rmlint/lib/checksum.c
[RM_DIGEST_XXHASH] = &xxhash_interface,
[RM_DIGEST_SHA256] = &sha256_interface,
[RM_DIGEST_BLAKE2B] = &blake2b_interface,
[RM_DIGEST_HIGHWAY256] = &highway256_interface,
[RM_DIGEST_PARANOID] = &paranoid_interface,
...
```

Non-paranoid mode compares digest outputs (fast). Paranoid mode stores and compares raw buffers progressively.

Paranoid optimization includes twin-candidate prematching + shadow hash:

```c
// /tmp/rmlint/lib/checksum.c
paranoid->shadow_hash = rm_digest_new(RM_DIGEST_XXHASH, 0);
...
if(rm_buffer_equal(buffer, paranoid->twin_candidate_buffer->data)) {
    paranoid->twin_candidate_buffer = paranoid->twin_candidate_buffer->next;
} else {
    paranoid->twin_candidate = NULL;
}
```

### 1.5 Reflink/hardlink/cross-device logic

`rm_util_link_type()` distinguishes same file/path double/hardlink/xdev/reflink via metadata + FIEMAP extent checks.

```c
// /tmp/rmlint/lib/utilities.c
if(stat1.st_dev == stat2.st_dev && stat1.st_ino == stat2.st_ino) {
    ... return RM_LINK_HARDLINK;
}
...
if(stat1.st_dev != stat2.st_dev && !rm_util_same_device(path1, path2))
    return RM_LINK_XDEV;
...
if(physical_1 == physical_2 ... is_last_1)
    return RM_LINK_REFLINK;
```

Shell formatter can emit clone/reflink/hardlink actions with guards:

```c
// /tmp/rmlint/lib/formats/sh.c.in
cp_reflink    'dupe' 'orig'
cp_hardlink   'dupe' 'orig'
clone         'dupe' 'orig'
```

### 1.6 Output structures (JSON + shell)

JSON output is array-based with header object, body items, and footer object:

```json
[
  {\"description\": \"...\", \"cwd\": \"...\", \"checksum_type\": \"blake2b\", ...},
  {\"id\": 123, \"type\": \"duplicate_file\", \"checksum\": \"...\", \"path\": \"...\", \"size\": 1234, ...},
  {\"aborted\": false, \"progress\": 100, \"total_files\": ..., \"duplicates\": ...}
]
```

From code (`json.c`): fields include `id`, `type`, `progress`, `checksum`, `path`, `size`, `inode`, `disk_id`, `is_original`, optional `hardlink_of`, and summary counters.

### 1.7 Memory management

rmlint uses several guards:

- staged reduction via preprocess before deep hashing
- explicit memory budgeting for paranoid mode (`paranoid_mem_alloc`)
- read-buffer quotas and semaphore-limited buffers in hasher
- estimated per-file overhead constant (`SHRED_AVERAGE_MEM_PER_FILE` = 100 bytes) for runtime budgeting

No evidence of external sort or mmap-based hashing in core path; it relies on in-memory GLib queues/lists/tables + bounded read buffers.

---

## 2) gdu deep dive (Go): goroutine scanning, tree model, progress pipeline

### 2.1 Scanning architecture

Default analyzer is parallel (`CreateAnalyzer()`), with recursive goroutine fan-out and a global limiter:

```go
// /tmp/gdu/pkg/analyze/parallel.go
var concurrencyLimit = make(chan struct{}, 3*runtime.GOMAXPROCS(0))
...
go func(entryPath string) {
    concurrencyLimit <- struct{}{}
    subdir := a.processDir(entryPath)
    subDirChan <- subdir
    <-concurrencyLimit
}(entryPath)
```

This is not a classic fixed worker pool over a job queue; it is recursive spawning bounded by a semaphore channel.

### 2.2 SSD optimization behavior

The project positions itself as SSD-first in CLI description and README:

- “intended primarily for SSD disks”
- `--sequential` flag recommended for rotating HDDs

```go
// /tmp/gdu/cmd/gdu/main.go
flags.BoolVar(&af.SequentialScanning, \"sequential\", false,
    \"Use sequential scanning (intended for rotating HDDs)\")
```

Important nuance: there is **no automatic SSD/HDD detection switching analyzer mode** in scan path; selection is user-configured.

### 2.3 Data structures

In-memory model is a classic mutable tree:

```go
// /tmp/gdu/pkg/analyze/file.go
type File struct {
    Mtime  time.Time
    Parent fs.Item
    Name   string
    Size   int64
    Usage  int64
    Mli    uint64
    Flag   rune
}

type Dir struct {
    *File
    BasePath  string
    Files     fs.Files
    ItemCount int
    m         sync.RWMutex
}
```

Stats are aggregated post-scan (`UpdateStats`), with hardlink-aware accounting via `HardLinkedItems` map.

### 2.4 Progressive TUI integration

The analyzer emits `CurrentProgress` through channels during scan; TUI consumes and renders “Scanning...” modal concurrently.

```go
// /tmp/gdu/tui/actions.go
go ui.updateProgress()
go func() {
    currentDir := ui.Analyzer.AnalyzeDir(path, ...)
    ui.topDir.UpdateStats(ui.linkedItems)
    ui.app.QueueUpdateDraw(func() { ... })
}()
```

```go
// /tmp/gdu/tui/progress.go
select {
case progress = <-progressChan:
case <-doneChan:
    ui.progress.SetTitle(\" Finalizing... \")
    return
}
```

This is a solid analyzer→UI streaming boundary for responsive UX.

### 2.5 Cross-platform handling

Platform-specific files define stat extraction differences:

- Linux/OpenBSD: `st.Blocks * 512`, `st.Mtim`
- macOS/BSD variants: `st.Blocks * 512`, `st.Mtimespec`
- Windows/Plan9: usage falls back to `info.Size()`

### 2.6 Memory efficiency and scale options

Core in-memory analyzer retains full tree; memory cost grows with entry count. For very large scans, gdu provides storage-backed analyzers:

- Badger (`StoredAnalyzer`)
- SQLite (`CreateSqliteAnalyzer`)

SQLite path is explicitly tuned for write-heavy ingestion (`PRAGMA synchronous=OFF`, `journal_mode=MEMORY`, prepared bulk insert statements).

This gives a practical “RAM vs speed” tradeoff path absent in many TUI scanners.

---

## 3) ncdu deep dive (C): efficient single-thread scan, compact nodes, JSON format

### 3.1 Scanning implementation

ncdu’s scanner (`dir_scan.c`) is single-threaded and deliberately simple:

- `chdir` into directory
- `opendir/readdir` names into a temporary NUL-separated buffer
- `lstat` each item
- recurse directories

The design comment explains why filenames are read first: avoid too many open FDs in deep recursion.

```c
// /tmp/ncdu/src/dir_scan.c
/* reason for reading everything in memory first ...
 * is to avoid eating too many file descriptors in a deeply recursive directory.
 */
```

This pattern performs well because it minimizes synchronization overhead and exploits kernel caches naturally.

### 3.2 Data structure and memory profile

Core node is compact, with flexible array member for name:

```c
// /tmp/ncdu/src/global.h
struct dir {
  int64_t size, asize;
  uint64_t ino, dev;
  struct dir *parent, *next, *prev, *sub, *hlnk;
  int items;
  unsigned short flags;
  char name[];
};
```

Memory sizing is explicit:

```c
// /tmp/ncdu/src/util.h
#define dir_memsize(n)     (offsetof(struct dir, name)+1+strlen(n))
#define dir_ext_memsize(n) (dir_ext_offset(n) + sizeof(struct dir_ext))
```

On 64-bit builds, fixed part is roughly ~80 bytes plus name (+ optional `dir_ext`). This is very efficient for in-memory browsing at scale.

### 3.3 Hardlink handling

Hardlinks are tracked using a hash set keyed by `(dev, ino)` (khash). Parent size accounting avoids double counting:

```c
// /tmp/ncdu/src/dir_mem.c
KHASHL_SET_INIT(... struct dir *, hlink_hash, hlink_equal)
...
if(!i) { /* already exists */ ... }
```

### 3.4 Incremental display during scan

UI updates are integrated in scan loop through `input_handle()` and `dir_draw()`:

- current item path
- item count
- total size
- warnings/errors
- abort interactions

This gives immediate progress despite single-thread execution.

### 3.5 Export/import JSON format

Exporter writes a versioned top-level array:

```json
[
  1,
  1,
  {\"progname\":\"ncdu\",\"progver\":\"...\",\"timestamp\":...},
  [ {\"name\":\"/\", ...}, [ ...subitems... ] ]
]
```

From `dir_export.c`:

- fields: `name`, `asize`, `dsize`, `dev`, `ino`, `uid/gid/mode/mtime` (if extended), `hlnkc`, `read_error`, `notreg`, `excluded`
- directories are represented as arrays `[item_info, child1, child2, ...]`

Importer (`dir_import.c`) validates major/minor version and reconstructs tree recursively.

### 3.6 Large filesystem handling

Two modes matter:

1. **Normal browse mode**: keeps tree in memory.
2. **Export mode (`-o`)**: streams output and does not keep full structure (documented explicitly), useful for remote or huge scans.

`ncdu.pod` also notes linear export size growth and higher memory when extended info is enabled (~30% increase).

---

## 4) squirreldisk deep dive (Rust/Tauri): backend orchestration around sidecar scanner

### 4.1 Scanning engine reality

The Rust backend does **not implement a full native walker/tree builder** in current code. It launches a sidecar binary (`pdu`) and forwards events.

```rust
// /tmp/squirreldisk/src-tauri/src/scan.rs
let (mut rx, child) = TauriCommand::new_sidecar(\"pdu\")
    .args(paths_to_scan)
    .spawn()?;
*state.0.lock().unwrap() = Some(child);
```

Arguments include:

- `--json-output`
- `--progress`
- `--min-ratio=<ratio>`
- path set (with special root handling and banned mount points)

### 4.2 Backend ↔ frontend communication

Backend streams sidecar output via Tauri events:

- stdout lines → `scan_completed` event (raw JSON chunks/string)
- stderr progress text parsed via regex → `scan_status` event with `{items,total,errors}`

```rust
let re = Regex::new(r\"\\(scanned ([0-9]*), total ([0-9]*)(?:, erred ([0-9]*))?\\)\")?;
...
app_handle.emit_all(\"scan_completed\", line).ok();
app_handle.emit_all(\"scan_status\", Payload { ... }).unwrap();
```

### 4.3 Data structures (backend)

Backend-local structs are minimal:

- `SquirrelDisk` for disk metadata (`sysinfo`)
- `Payload` for progress updates
- shared process state `Mutex<Option<CommandChild>>` for stop/kill

There is no arena/tree model in Rust backend for scan results; result tree lifecycle is externalized to sidecar output and frontend consumption.

### 4.4 Cross-platform support

- disk listing via `sysinfo`
- OS-specific “show in folder” commands (`explorer`, `xdg-open`, `open -R`)
- sidecar binaries bundled for multiple targets (`src-tauri/bin/pdu-*`)

### 4.5 Performance implications

This architecture is pragmatic:

- scanner performance depends on sidecar (`parallel-disk-usage`)
- Tauri backend remains lightweight and maintainable
- hard part (fast parallel walk + aggregation) is delegated

For a cleaner product, this pattern is useful if you want pluggable scan engines per platform.

---

## 5) Cross-comparison table

| Aspect | rmlint | gdu | ncdu | squirreldisk |
|---|---|---|---|---|
| Language | C | Go | C | Rust/Tauri (backend) + sidecar scanner |
| Primary purpose | Duplicate/lint cleaner | Disk usage analyzer (CLI/TUI) | Disk usage analyzer (ncurses) | GUI analyzer |
| Scanning parallelism | Yes: MDS per-disk queues + thread pools | Yes: recursive goroutines, bounded by `3*GOMAXPROCS` | No (single-threaded) | Backend delegates to sidecar (`pdu`) |
| Duplicate detection | Yes, core feature | No | No | No (depends on sidecar; current path is usage scan) |
| Candidate reduction | Strong (size groups + inode/path dedup) | N/A dedup | N/A dedup | N/A dedup |
| Incremental verification | Yes (progressive hash offsets) | N/A | N/A | N/A |
| Data structure | GLib queues/lists/hash tables + `RmFile`/groups | `File`/`Dir` tree in memory; optional DB-backed analyzers | Compact `struct dir` tree with flexible name storage | Minimal backend structs; scan tree external |
| Memory per file (qualitative) | Moderate, with budgeting and pruning | Higher in in-memory mode; mitigated by SQLite/Badger modes | Low (compact node + name) | Backend low; frontend/sidecar carry data load |
| Export format | JSON + shell scripts (actionable cleanup) | JSON import/export report support | Versioned JSON tree export/import | Emits sidecar JSON over events |
| SSD optimization | Device-aware scheduling; rotational handling in MDS | SSD-first by design; user chooses `--sequential` for HDD | None explicit; relies on sequential scan behavior | Depends on sidecar behavior |
| Cross-platform | Linux/macOS focus, Unix-heavy internals | Linux/macOS/BSD/Windows support | Unix-like focus | Linux/macOS/Windows via Tauri |

---

## 6) Recommendations for a disk scanner + future cleaner

### 6.1 Data structure recommendations

1. **Split metadata from heavy path storage**
   - Use compact fixed-size node records (inode/dev/size/flags/parent index) plus external string arena/path interning.
   - ncdu’s compact node philosophy is a good baseline.

2. **Use staged representations by phase**
   - Phase A (scan): append-only node table + parent links.
   - Phase B (candidate filtering): index by `(size, maybe extension, maybe mtime bucket)`.
   - Phase C (dedupe verify): move to small active worksets.
   - This mirrors rmlint’s preprocess→shred progression.

3. **Offer storage-backed mode early**
   - gdu’s SQLite/Badger option is a practical escape hatch for huge trees.
   - For cleaner UX, hybrid mode (hot set in memory, cold set on disk) avoids OOM without sacrificing responsiveness.

### 6.2 Duplicate detection strategy (from rmlint)

Adopt a multi-stage, fail-fast pipeline:

1. **Exact metadata sieve**: type, size, exclusion rules, inode/hardlink/path-double normalization.
2. **Progressive hashing**:
   - Start with small prefix increments.
   - Expand read windows only for surviving collisions.
   - Stop once groups collapse to uniques.
3. **Optional paranoid verification** for delete-grade confidence.
4. **Device-aware scheduling**:
   - Keep HDD seeks low (offset-aware queues).
   - More aggressive concurrency on SSD.

This gives cleaner-grade safety while minimizing unnecessary I/O.

### 6.3 Output format recommendations (from ncdu + rmlint)

Use two output layers:

- **Analysis JSON** (replayable/importable): versioned schema, metadata header, item stream/tree, footer stats.
- **Action plan output** (cleaner): explicit operation records (`remove`, `hardlink`, `reflink`, `move-to-trash`) with dry-run status and rollback metadata.

rmlint’s shell formatter demonstrates why actionable outputs are critical for trust and automation.

### 6.4 Scanning-to-UI pipeline recommendations (from gdu + squirreldisk)

1. **Progress channel/event bus** with coarse-grained counters and current path.
2. **Dual stream**:
   - progress telemetry (small, frequent)
   - result chunks (larger, less frequent)
3. **Cancelable process model**:
   - keep scan task handle in shared state and support hard kill + graceful stop.
4. **Do not block UI on final aggregation**:
   - render partial trees early, refine weights/stats incrementally.

### 6.5 Cleaner-specific architecture blueprint

- **Scanner service** (native core): file walk + metadata capture + dedupe candidate indexing.
- **Verifier service**: incremental hashing and optional byte-compare.
- **Planner**: choose originals by policy (mtime, path priority, tag/keep rules).
- **Executor**: operations with transaction log and post-checks.
- **Reporter**: JSON + human-friendly script/log.

This separates concerns and keeps safety-critical delete logic isolated.

---

## 7) Key takeaways

### For disk scanner (analyzer)

- Keep scan core simple and cache-friendly (ncdu lesson).
- Provide high-parallel mode with bounded concurrency (gdu lesson).
- Stream progress and partial results continuously (gdu/squirreldisk lesson).
- Add storage-backed mode for large trees (gdu lesson).

### For future cleaner

- Build duplicate detection around **candidate reduction + progressive verification** (rmlint lesson).
- Include filesystem-aware link semantics (hardlink/reflink/xdev) before destructive actions.
- Generate reproducible, auditable action plans.
- Offer paranoid mode for users requiring maximal correctness.

### Practical implementation order

1. Analyzer tree + progress channels + import/export.
2. Candidate indexing and policy engine.
3. Incremental hasher + verifier pipeline.
4. Safe executor (dry-run, transactional logs, rollback strategy).
5. UI affordances for trust: explain “why duplicate”, preview action impact, and expose reversible operations where possible.

---

## Appendix: selected reference snippets

### rmlint: preprocess groups

```c
// /tmp/rmlint/lib/preprocess.c
/* After rm_preprocess(), all remaining duplicate candidates are in
 * session->tables->size_groups->group1->file1a ...
 */
```

### gdu: bounded parallel recursion

```go
// /tmp/gdu/pkg/analyze/parallel.go
var concurrencyLimit = make(chan struct{}, 3*runtime.GOMAXPROCS(0))
```

### ncdu: compact per-item allocation

```c
// /tmp/ncdu/src/util.h
#define dir_memsize(n) (offsetof(struct dir, name)+1+strlen(n))
```

### squirreldisk: sidecar orchestration

```rust
// /tmp/squirreldisk/src-tauri/src/scan.rs
TauriCommand::new_sidecar(\"pdu\").args(paths_to_scan).spawn()?;
```