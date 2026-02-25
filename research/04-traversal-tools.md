# Research Report: Fast Filesystem Traversal Architectures (bfs, fd, fuc)

This report analyzes three high-performance filesystem traversal implementations from different design lineages:

- **bfs** (C): async I/O queue + traversal engine
- **fd** (Rust): highly-optimized filtering over `ignore::WalkParallel`
- **fuc** (Rust): Linux-first operations (`rmz`/`cpz`) built around low-level `rustix` APIs and explicit scheduling

The emphasis is on traversal hot paths, syscall behavior, queueing/scheduling models, and what can be reused in a high-performance disk scanner.

---

## Executive summary (5 key findings)

1. **bfs’s core advantage is not just threads; it is queue decoupling with atomic MPMC rings and batched request/response flow.**
   The main thread enqueues `opendir/stat/close` work, I/O workers execute it, and completions are reattached to traversal queues. In best cases, pop/push are mostly atomic operations and cache-friendly batched accesses.

2. **fd’s speed is a composition of three things: early pruning (ignore rules), parallel DFS work stealing, and low-cost type-first filtering.**
   The `ignore` crate avoids expensive post-filtering by integrating pattern decisions in traversal itself; fd then layers additional filters in a “cheap-first, expensive-later” order.

3. **fuc’s Linux path is aggressively system-call-oriented and operation-specific, not generic traversal.**
   It uses `rustix::fs::RawDir` (getdents-like streaming), `openat`, `unlinkat`, `copy_file_range`, `statx`, and explicit thread scheduling over channels. Its parent-pointer deletion tree solves bottom-up delete ordering efficiently.

4. **Traversal order decisions are architectural, not cosmetic.**
   bfs defaults to breadth-first (with configurable strategies), ignore/fd workers intentionally choose depth-first local stacks for memory behavior, and fuc uses task-driven recursive work units tuned for deletion/copy semantics.

5. **The strongest “ideal” architecture combines bfs-style async request/completion queues, ignore-style integrated pruning, and fuc-style dirfd-relative syscall discipline.**
   A next-gen Rust scanner should not copy one tool wholesale; it should hybridize these traits into a two-plane design: (A) traversal+matching plane and (B) asynchronous I/O execution plane.

---

## 1) bfs deep dive (C): async I/O queue + breadth-first traversal engine

### 1.1 What bfs optimized beyond classic `find`

bfs is architected around a dedicated traversal core (`bftw`) plus an asynchronous I/O queue (`ioq`) used for directory opens, closes, and stats. The key optimization is **latency hiding**:

- main traversal thread continues queue orchestration and callback processing
- background thread(s) execute blocking I/O
- completions are consumed in batches and rejoined with traversal state

This makes traversal less “call stack blocked on syscalls” and more “pipeline with inflight I/O depth.”

### 1.2 ioq architecture: dual queues + slot tags + monitor pool

At top of `src/ioq.c`, bfs documents the queue model explicitly:

```c
/* struct ioq is composed of two separate queues:
 *   struct ioqq *pending; // Pending I/O requests
 *   struct ioqq *ready;   // Ready I/O responses
 */
```

Each `ioqq` is a circular-buffer MPMC queue with fetch-add indexing:

```c
struct ioqq {
    size_t slot_mask;
    size_t monitor_mask;
    struct ioq_monitor *monitors;
    cache_align atomic size_t head;
    cache_align atomic size_t tail;
    cache_align ioq_slot slots[];
};
```

Core behavior:

- push side: `fetch_add(&head, ...)` then slot CAS
- pop side: `fetch_add(&tail, ...)` then slot CAS
- nonblocking readers may race ahead; bfs solves this with **skip counters encoded in tag bits** (`IOQ_SKIP`)
- sleeping waiters are tracked by `IOQ_BLOCKED` and awakened via monitor pool (mutex+condvar)

Slot state machine (simplified):

- empty → full(ptr)
- empty → skip(1) for reader-ahead case
- skip(n) increment/decrement semantics allow wraparound correctness
- blocked bit triggers wakeups after slot transitions

Representative push/pop internals:

```c
static void ioqq_push(struct ioqq *ioqq, struct ioq_ent *ent) {
    while (true) {
        size_t i = fetch_add(&ioqq->head, 1, relaxed);
        ioq_slot *slot = &ioqq->slots[i & ioqq->slot_mask];
        if (ioq_slot_push(ioqq, slot, ent)) {
            break;
        }
    }
}

static void ioqq_pop_batch(struct ioqq *ioqq, struct ioq_ent *batch[], size_t size, bool block) {
    size_t mask = ioqq->slot_mask;
    size_t i = fetch_add(&ioqq->tail, size, relaxed);
    for (size_t j = i + size; i != j; ++i) {
        ioq_slot *slot = &ioqq->slots[i & mask];
        *batch++ = ioq_slot_pop(ioqq, slot, block);
        block = false;
    }
}
```

Important detail: bfs also does **cache-line-sized batching** (`IOQ_BATCH`) to reduce queue traffic overhead.

### 1.3 `ioq_opendir()`, `ioq_closedir()`, `ioq_pop()` lifecycle

Request APIs are very small wrappers around queue entry creation + batch push:

```c
int ioq_opendir(struct ioq *ioq, struct bfs_dir *dir, int dfd, const char *path, enum bfs_dir_flags flags, void *ptr) {
    struct ioq_ent *ent = ioq_request(ioq, IOQ_OPENDIR, ptr);
    ...
    ioq_batch_push(ioq->pending, &ioq->pending_batch, ent);
    return 0;
}

int ioq_closedir(struct ioq *ioq, struct bfs_dir *dir, void *ptr) {
    struct ioq_ent *ent = ioq_request(ioq, IOQ_CLOSEDIR, ptr);
    ...
    ioq_batch_push(ioq->pending, &ioq->pending_batch, ent);
    return 0;
}

struct ioq_ent *ioq_pop(struct ioq *ioq, bool block) {
    bfs_assert(ioq_batch_empty(&ioq->pending_batch));
    if (ioq->size == 0) return NULL;
    return ioq_batch_pop(ioq->ready, &ioq->ready_batch, block);
}
```

In `bftw`, `bftw_ioq_pop()` handles completion semantics and reintegration:

```c
switch (op) {
case IOQ_OPENDIR:
    if (ent->result >= 0) { bftw_file_set_dir(cache, file, ent->opendir.dir); }
    else { bftw_freedir(cache, ent->opendir.dir); }
    bftw_queue_attach(&state->dirq, file, true);
    break;

case IOQ_STAT:
    ...
    bftw_queue_attach(&state->fileq, file, true);
    break;
}
```

So the async queue is tightly coupled to traversal queues (`dirq`, `fileq`) via detach/attach transitions.

### 1.4 Thread pool model and limits

Thread defaults are capped from CPU count in context setup:

```c
ctx->threads = nproc();
if (ctx->threads > 8) {
    ctx->threads = 8; // Not much speedup after 8 threads
}
```

Traversal uses one main thread + background I/O workers (`ctx->threads - 1`):

```c
int nthreads = ctx->threads - 1;
```

And workers are created during `ioq_create(depth, nthreads)`. So the practical model is:

- 1 thread: orchestration/callbacks/traversal logic
- up to 7 background I/O threads by default (if cap=8 total)

### 1.5 Main-thread/worker synchronization model

Synchronization is mixed:

- **C11 atomics** (`load/store/fetch_add/CAS`) for queue slots and counters
- **mutex/condvar** monitor pool for blocking waits and wakeups
- queue batching to amortize synchronization overhead

`ioq_slot_wait()` spins briefly with exponential backoff, then blocks on monitor condvar; `ioq_slot_wake()` emits broadcast when blocked bit was observed.

This gives a low-latency fast path (atomics only) plus bounded blocking fallback.

### 1.6 io_uring integration in bfs

bfs already has production io_uring support (`BFS_WITH_LIBURING`) in `src/ioq.c`:

- probes supported ops (`IORING_OP_OPENAT`, `IORING_OP_CLOSE`, `IORING_OP_STATX`)
- uses setup optimizations when available:
  - `IORING_SETUP_ATTACH_WQ`
  - `IORING_SETUP_SUBMIT_ALL`
  - `IORING_SETUP_R_DISABLED`
  - `IORING_SETUP_SINGLE_ISSUER`
  - `IORING_SETUP_DEFER_TASKRUN`
- limits io-wq workers via `io_uring_register_iowq_max_workers`

Dispatch path:

```c
case IOQ_OPENDIR:
    io_uring_prep_openat(...)
case IOQ_CLOSEDIR:
    io_uring_prep_close(...)
case IOQ_STAT:
    io_uring_prep_statx(...)
```

Important current gap is explicit in code:

```c
// TODO: io_uring_prep_getdents()
```

So bfs still performs directory entry reads through `bfs_polldir()/getdents` path after open completion; there is no integrated uring `getdents` operation yet.

Repository/PR state:

- io_uring landed in PR #106 (`io_uring`, merged)
- project planning issue tracks `getdents()` as pending “if added to io_uring”

### 1.7 Directory traversal mechanics (bftw)

`bftw` uses staged queues with optional ordering/buffering semantics:

- `buffer` list
- `waiting` list (awaiting service)
- `ready` list (service complete)

Queue flags control behavior:

- `BFTW_QLIFO` (DFS-like stack behavior)
- `BFTW_QBUFFER` (batch before enqueue)
- `BFTW_QORDER` (strict order)
- `BFTW_QBALANCE` (sync/async fairness)

Strategy selection:

```c
if (state->strategy != BFTW_BFS) {
    qflags |= BFTW_QBUFFER | BFTW_QLIFO;
}
```

So BFS is explicit default behavior; DFS/other strategies alter queue semantics.

Mount boundaries:

`bftw_is_mount()` compares device IDs (`statbuf->dev != parent->dev`) and `BFTW_SKIP_MOUNTS` / `BFTW_PRUNE_MOUNTS` gate traversal accordingly.

### 1.8 Syscalls and low-level I/O in bfs

Traversal-relevant syscalls in hot paths include:

- `openat` for directory opens (`bfs_opendir` and bftw open helpers)
- `getdents` / `getdents64` / `syscall(SYS_getdents64)` in `dir.c`
- `fstatat` and `statx` (with fallback) in `stat.c`
- `close`/`closedir` and fd unwrapping variants
- plus io_uring submissions for `openat/close/statx` where supported

### 1.9 Buffer/memory management for directory reads

In `dir.c`, bfs allocates a fixed-size directory object with embedded read buffer on getdents platforms:

```c
#define DIR_SIZE (64 << 10)
#define BUF_SIZE (DIR_SIZE - sizeof(struct bfs_dir))

struct bfs_dir {
    int fd;
    unsigned short pos;
    unsigned short size;
    alignas(sys_dirent) char buf[];
};
```

Notable behavior:

- `bfs_polldir()` fills buffer via getdents
- attempts an eager second read if room remains, to reduce one trailing syscall at EOF
- converts d_type to internal type and skips `.`/`..`

This design minimizes per-entry allocations and keeps directory stream state compact and cache-local.

---

## 2) fd deep dive (Rust): `ignore::WalkParallel` + aggressive in-walk filtering

### 2.1 fd integration with `ignore` crate

fd delegates traversal to `ignore` but configures it heavily via `WalkBuilder`:

```rust
let mut builder = WalkBuilder::new(first_path);
builder
    .hidden(config.ignore_hidden)
    .ignore(config.read_fdignore)
    .parents(config.read_parent_ignore && (config.read_fdignore || config.read_vcsignore))
    .git_ignore(config.read_vcsignore)
    .git_global(config.read_vcsignore)
    .git_exclude(config.read_vcsignore)
    .require_git(config.require_git_to_read_vcsignore)
    .overrides(overrides)
    .follow_links(config.follow_links)
    .same_file_system(config.one_file_system)
    .max_depth(config.max_depth);

let walker = builder.threads(config.threads).build_parallel();
```

Thread count in fd CLI defaults to available parallelism capped at 64, then passed into ignore’s walker.

### 2.2 `ignore::WalkParallel` internals: work-stealing DFS

The key internals are in `ripgrep/crates/ignore/src/walk.rs`.

`WalkParallel::visit`:

- seeds root work items
- creates per-thread workers and stacks
- each worker runs producer+consumer loop

Work distribution is explicit work stealing via crossbeam deque:

```rust
struct Stack {
    index: usize,
    deque: Deque<Message>,
    stealers: Arc<[Stealer<Message>]>,
}
```

Critical traversal-order choice:

```rust
// Using new_lifo() ensures each worker operates depth-first, not breadth-first.
// ... breadth first traversal on wide directories with a lot of gitignores is disastrous.
Deque::new_lifo
```

That comment captures a major practical insight: depth-first local traversal keeps ignore matcher/path state bounded better than broad BFS expansion.

### 2.3 Filtering during walk, not after

fd’s sender closure applies filters in staged order as each entry arrives from ignore walker:

1. skip roots (`depth == 0`)
2. optional ignore-contain short-circuit
3. regex/pattern matching on filename/full path
4. extension filter
5. file type filter via `file_type()` (cheap)
6. ownership filter (metadata needed)
7. size filter (metadata needed)
8. mtime filter (metadata needed)
9. optional style precomputation
10. emit result / prune logic

This is **fundamentally different from “collect all then filter”**: it avoids unnecessary metadata and output work early.

### 2.4 How ignore matching is done efficiently

`ignore` maintains per-directory matcher state (`Ignore`), and each work item carries its parent matcher context.

`should_skip_entry()`:

```rust
fn should_skip_entry(ig: &Ignore, dent: &DirEntry) -> bool {
    let m = ig.matched_dir_entry(dent);
    if m.is_ignore() { true }
    else if m.is_whitelist() { false }
    else { false }
}
```

During descent, workers call:

- `add_parents()` for root-depth inherited rules
- `add_child()` when entering a directory

This yields correct precedence over `.ignore`, `.gitignore`, global ignore, overrides, and nesting hierarchy.

### 2.5 DirEntry/type metadata behavior and syscall implications

fd wraps `ignore::DirEntry` in its own `DirEntry` with lazy metadata cache (`OnceCell<Option<Metadata>>`):

```rust
pub fn file_type(&self) -> Option<FileType> {
    match &self.inner {
        DirEntryInner::Normal(e) => e.file_type(),
        DirEntryInner::BrokenSymlink(_) => self.metadata().map(|m| m.file_type()),
    }
}
```

and

```rust
pub fn metadata(&self) -> Option<&Metadata> {
    self.metadata.get_or_init(|| ...).as_ref()
}
```

Implication:

- file-type and path-based filters run before metadata fetches
- expensive stats only happen when needed (size/owner/time/etc.)

Inside `ignore` parallel walk, entries come from `std::fs::read_dir` + `DirEntry::file_type()` first. On Linux/glibc this often maps to `d_type`-driven fast path where possible; when type cannot be trusted/known, library layers may fall back to metadata calls.

### 2.6 Why fd gets large speedups in practice

The “23x vs find -iregex” benchmark in fd README is a compound effect:

- parallel traversal workers
- integrated ignore pruning (fewer dirs/files traversed in default mode)
- efficient regex engine (`regex` crate)
- cheap-first filtering before metadata
- predictable memory behavior from DFS LIFO + work stealing

For unrestricted mode (`-u`) speedup narrows but still tends to remain significant due to traversal and filtering architecture.

---

## 3) fuc deep dive (Rust): Linux-focused low-level traversal for `rmz`/`cpz`

### 3.1 What fuc is architecturally

fuc is not a generic `find` clone. It is an operation engine for copy/remove where traversal is tightly coupled to operation semantics:

- remove must preserve ordering constraints (delete children before parent)
- copy must preserve metadata/link behavior and recursion safety

### 3.2 “Raw syscall” usage pattern (practical interpretation)

On Linux, fuc uses `rustix` APIs heavily:

- `RawDir` (directory stream over low-level kernel interfaces)
- `openat`, `unlinkat`, `mkdirat`, `readlinkat`, `statx`, `linkat`, `copy_file_range`
- `unshare_unsafe(UnshareFlags::FILES | UnshareFlags::FS)` or FILES-only for copy path

Example deletion open/read loop:

```rust
let dir = openat(CWD, &node.path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW, Mode::empty())?;
let mut raw_dir = RawDir::new(&dir, buf);
while let Some(file) = raw_dir.next() {
    ...
}
```

Example copy fast-path:

```rust
copy_file_range(&from, None, &to, None, ...)
```

So it bypasses high-level std traversal APIs in Linux hot paths. Strictly speaking, this is through `rustix` wrappers (not handwritten `libc::syscall` in fuc code), but it is still syscall-oriented and close to kernel semantics.

### 3.3 Buffer management for directory reads

Both remove/copy Linux workers use fixed stack buffers for raw dir iteration:

```rust
let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
let mut raw_dir = RawDir::new(&dir, buf);
```

Characteristics:

- per-thread stack buffer avoids frequent heap churn
- small, deterministic memory footprint per worker
- tight loops over raw dir entries with immediate dispatch decisions

### 3.4 Parent-pointer tree in `rmz`

The remove engine builds a node chain with parent pointers:

```rust
struct TreeNode {
    path: CString,
    parent: Option<Arc<Self>>,
    messages: Sender<Self>,
}
```

When descending, child tasks clone parent Arc; when done, deletion unwinds upward:

```rust
fn delete_empty_dir_chain(mut node: Option<TreeNode>) -> Result<(), Error> {
    while let Some(TreeNode { path, parent, .. }) = node {
        unlinkat(CWD, path, AtFlags::REMOVEDIR)?;
        node = parent.and_then(Arc::into_inner);
    }
}
```

This elegantly solves the recursive deletion ordering problem with minimal synchronization.

### 3.5 Parallel traversal/work distribution in fuc

fuc uses a root worker + dynamic worker spawn model over `crossbeam_channel`:

- root thread consumes tasks
- if more parallelism available and queue non-empty, spawns another worker
- workers consume same queue and process directories

Representative pattern:

```rust
let mut available_parallelism = thread::available_parallelism().map_or(1, NonZeroUsize::get) - 1;

for message in &tasks {
    let mut maybe_spawn = || {
        if available_parallelism > 0 && !tasks.is_empty() {
            available_parallelism -= 1;
            threads.push(scope.spawn({ let tasks = tasks.clone(); || worker_thread(tasks) }));
        }
    };
    maybe_spawn();
    delete_dir(message, &mut buf, maybe_spawn)?;
}
```

This is less general than work-stealing deques but highly direct for “task = directory subtree op.”

### 3.6 Traversal-deletion and traversal-copy correctness details

`rmz`:

- deletes non-directories eagerly
- if encountered `ISDIR` unexpectedly, schedules subtree
- handles long path fallback by changing cwd and calling `remove_dir_all` fallback
- finally removes directories bottom-up through parent chain

`cpz`:

- detects recursion with inode guard (`root_to_inode`) to avoid copying directory into itself
- if `file_type` unknown or symlink-follow required, calls `statx` (`get_file_type`)
- for regular files: `copy_file_range` fast path, fallback to `io::copy` on cross-device (`EXDEV`)
- symlink copy via `readlinkat` + `symlinkat`

### 3.7 io_uring status in fuc

No io_uring implementation in current code paths.

Repository issue exists requesting io_uring adoption, but blocked by missing/insufficient operation support for intended copy acceleration use cases.

---

## 4) Cross-cutting comparison

| Aspect | bfs (C) | fd (Rust) | fuc (Rust) |
|---|---|---|---|
| Primary goal | Fast `find`-compatible traversal/query execution | Fast user-friendly find alternative | Fast copy/remove ops (`cpz`, `rmz`) |
| Language/runtime | C + pthread + atomics + optional liburing | Rust + `ignore` + crossbeam channels | Rust + `rustix` + crossbeam channels |
| Parallelism model | Dedicated async I/O queue with background workers | Parallel traversal workers via `WalkParallel` | Dynamic worker spawning over task channel |
| Traversal order | Configurable; BFS default, DFS/others supported | DFS per worker (LIFO), global work stealing | Task-driven subtree processing |
| Async strategy | Explicit request/response queue (`pending`/`ready`) | No separate async I/O queue; parallel walkers process entries directly | No separate async queue; workers do syscall loops directly |
| Queue internals | Custom MPMC circular queue with slot tags (`IOQ_SKIP`, `IOQ_BLOCKED`) + monitor pool | crossbeam deque work stealing inside ignore crate | crossbeam unbounded channel + on-demand thread spawn |
| Filtering timing | During traversal/callback pipeline; async stat integration | During walk callback, cheap-first ordering | Operation-specific decisions in traversal loop |
| Syscall style | `openat`, `getdents*`, `fstatat/statx`, `close`; io_uring for open/close/statx | Primarily std + ignore internals (`read_dir`, `file_type`, metadata as needed) | `openat`, `RawDir`, `unlinkat`, `mkdirat`, `statx`, `copy_file_range`, `linkat`, `readlinkat` |
| d_type exploitation | Yes via getdents and `de->d_type` conversion | Via library stack (`DirEntry::file_type`) when available | Yes in `RawDir` entries; fallback statx when unknown |
| io_uring | Implemented for open/close/statx; getdents TODO | None in traversal path | Planned only (issue), not implemented |
| Buffer strategy | 64 KiB per bfs_dir object for getdents buffers | Library-managed; metadata lazily fetched | 8 KiB stack buffers per worker for `RawDir` |
| Mount boundaries | Device-ID checks + prune/skip mount options | `same_file_system` support in ignore walker | Operation-specific inode/device handling (e.g., recursion guard) |
| Key innovation | Decoupled async I/O pipeline integrated with traversal scheduler | Integrated ignore-aware parallel DFS walker | Operation-specialized low-level traversal with parent-pointer deletion chain |

---

## 5) Architectural lessons: what to steal from each

### From bfs

1. **Separate control plane from I/O execution plane.**
   Keep traversal state machine independent from syscall completion timing.

2. **Batch queue operations aggressively.**
   Small batching in hot path can be large throughput gain under contention.

3. **Represent async completion as explicit state transitions.**
   Detached/inflight/ready states in traversal queue avoid ad hoc synchronization.

4. **Tune thread model by measured diminishing returns.**
   bfs’s 8-thread cap is a pragmatic signal: balance speedup and overhead.

### From fd/ignore

1. **Do filtering as early as possible in traversal.**
   Don’t collect first then filter.

2. **Order predicates by expected cost and selectivity.**
   Name/path/type first, metadata later.

3. **Prefer DFS locality + work stealing over global BFS explosion.**
   Especially important with large rule contexts (`.gitignore` chains).

4. **Keep ignore matcher state incremental per directory.**
   Rebuilding patterns globally is too expensive.

### From fuc

1. **Use dirfd-relative syscalls in recursive operations.**
   `openat`/`unlinkat`/`statx` avoid repeated path resolution and reduce race exposure.

2. **Model recursive ordering constraints explicitly in data structures.**
   Parent-pointer chains are an efficient answer for post-order delete.

3. **Use stack buffers + low-allocation loops for directory entry scans.**
   Keeps allocator out of the hottest traversal loops.

4. **Operation-specific fallback logic matters.**
   `copy_file_range` fallback paths and long-path fallbacks prevent perf wins from becoming correctness regressions.

---

## 6) Synthesized “ideal traversal architecture”

For a high-performance disk scanner in Rust, a synthesized architecture should be:

### 6.1 Two-plane model

- **Traversal/matching plane (main scheduler):**
  - owns graph/queue state
  - applies prune/match policy
  - controls ordering semantics
- **I/O execution plane (worker pool):**
  - executes open/read/stat operations
  - returns typed completions

This mirrors bfs’s strongest pattern while staying Rust-native.

### 6.2 Dataflow

1. seed root work items
2. schedule directory open requests
3. workers return `DirBatch` completions (entries + d_type + errors)
4. scheduler applies cheap filters, emits matches, queues descendants
5. scheduler requests metadata only for survivors requiring expensive predicates
6. optional close/defer-close requests pipelined asynchronously

### 6.3 Work queues

Use two levels:

- global MPMC request/response queues (bfs pattern)
- worker-local LIFO stacks with stealing for subtree expansion (ignore pattern)

This can preserve DFS locality while still decoupling blocking syscalls.

### 6.4 Syscall strategy

On Linux, prefer dirfd-relative and low-level APIs:

- directory reads through `RawDir`/getdents-like primitives
- `openat2/openat`, `statx`, `unlinkat` where relevant
- avoid path re-resolution by carrying parent dirfd context

### 6.5 Filtering pipeline

Make filter stages explicit with cost tiers:

- Tier 0: static config prune (depth/fs boundary)
- Tier 1: name/path/glob/ignore matcher
- Tier 2: file type (from entry metadata/d_type)
- Tier 3: metadata-dependent predicates (size/time/owner/hash)

Only escalate tiers when needed.

### 6.6 Completion contracts

Completions should be typed and minimal:

- `OpenDirOk { token, dir_handle }`
- `ReadDirOk { token, entries }`
- `StatOk { token, stat }`
- `IoErr { token, errno, phase }`

Avoid “opaque callback” style for core scheduler correctness and observability.

### 6.7 io_uring use policy

Use io_uring opportunistically for operations with clear win and mature kernel support in target fleet:

- currently good for open/close/statx
- keep fallback parity and capability probing
- do not block architecture on unmerged/premature operations like getdents support

### 6.8 Memory model

- stack/arena allocation in hot loops
- bounded per-worker reusable buffers
- compact entry representation (name slices + type bits + optional inode)
- delayed path materialization (construct full path only if needed for output)

---

## 7) Recommended Rust implementation patterns

### 7.1 Queue and scheduler patterns

- Use lock-free MPMC channel/ring for request and completion transport.
- Use worker-local LIFO deques for traversal expansion and work stealing.
- Batch both submits and completion drains.

### 7.2 Entry representation

```rust
struct EntryLite<'a> {
    parent_token: u32,
    name: &'a [u8],
    file_type_hint: FileTypeHint,
    depth: u16,
    ino: Option<u64>,
}
```

Keep this lightweight and avoid heap allocations for common case.

### 7.3 Metadata fetch discipline

- `OnceCell`/lazy metadata per entry (fd pattern)
- request stat only if downstream predicates demand it
- cache stat result by token+flags

### 7.4 Ignore/rules engine integration

- carry compiled matcher state per directory context
- update matcher only on directory boundaries
- apply matcher before scheduling child traversal

### 7.5 Error strategy

- attach operation phase + path/token context
- continue on recoverable errors with configurable policy
- keep scheduler resilient; do not panic in worker loops

### 7.6 Linux-first optional accelerations

- `rustix` for clear syscall mapping and portability envelope
- `statx` for richer metadata and mount-id support
- `copy_file_range`-style specialization only in copy-related pipelines

### 7.7 Instrumentation and tuning

Expose counters at runtime:

- queue depth percentiles
- inflight I/O count
- stat requests avoided due to type hints
- matcher prune rate
- entries/sec per depth bucket

Without these metrics, most optimization decisions are guesswork.

---

## 8) Key takeaways for our disk scanner

1. **Adopt a bfs-like asynchronous I/O queue boundary, but in Rust.**
   Keep traversal logic deterministic while allowing syscall overlap.

2. **Adopt ignore/fd-style integrated pruning and predicate ordering.**
   This can remove huge volumes of unnecessary work before metadata calls.

3. **Adopt fuc-style dirfd-relative low-level operations for Linux fast path.**
   This reduces path lookup overhead and improves correctness under churn.

4. **Choose DFS-local work expansion with stealing, not naive BFS fanout.**
   Better memory behavior on deep/wide trees and rule-heavy repos.

5. **Treat metadata as an expensive tier.**
   Build pipeline so most entries never need `stat`.

6. **Use explicit completion tokens and typed events.**
   This simplifies cancellation, retries, and diagnostics.

7. **Integrate mount/device boundaries from day one.**
   Device-aware traversal affects correctness and performance significantly.

8. **Plan io_uring as an optimization module, not a dependency.**
   Probe capabilities, keep robust synchronous fallbacks.

9. **Implement reusable per-worker buffers and arenas.**
   Keep allocator pressure low in directory hot loops.

10. **Benchmark by workload class, not one synthetic test.**
    At minimum: warm cache, cold cache, deep trees, wide trees, ignore-heavy trees, early-exit searches, and metadata-heavy predicates.

---

## Appendix: concise hot-path excerpts

### bfs: queue request path

```c
struct ioq_ent *ent = ioq_request(ioq, IOQ_OPENDIR, ptr);
ent->opendir.dir = dir;
ent->opendir.dfd = dfd;
ent->opendir.path = path;
ioq_batch_push(ioq->pending, &ioq->pending_batch, ent);
```

### bfs: queue completion consumption in traversal

```c
ioq_submit(ioq);
struct ioq_ent *ent = ioq_pop(ioq, block);
... bftw_queue_attach(&state->dirq, file, true);
```

### ignore: DFS + work stealing rationale

```rust
// Using new_lifo() ensures each worker operates depth-first, not breadth-first.
// ... breadth first traversal on wide directories with a lot of gitignores is disastrous.
```

### fd: cheap-first filtering

```rust
// Check the name first, since it doesn't require metadata
... extension filter ...
... file type filter ...
... owner/size/time filters (metadata) ...
```

### fuc rmz: parent-pointer deletion chain

```rust
struct TreeNode { path: CString, parent: Option<Arc<Self>>, messages: Sender<Self> }
...
unlinkat(CWD, path, AtFlags::REMOVEDIR)
node = parent.and_then(Arc::into_inner);
```

### fuc cpz: low-level copy fast path

```rust
let mut raw_dir = RawDir::new(&from_dir, buf);
... copy_file_range(&from, None, &to, None, ...)
```
