@@ -0,0 +1,648 @@
+# Rust Parallel Filesystem Walking Libraries: Internal Architecture Research
+
+## Executive summary (5 key findings)
+
+1. **There are two fundamentally different parallel-walk designs in this set**:  
+   - `jwalk`: parallelize `read_dir` jobs while preserving a deterministic DFS output order using an explicit ordered queue and index-path bookkeeping.  
+   - `ignore::WalkParallel`: custom work-stealing deques (`crossbeam_deque`) with per-thread visitors and asynchronous delivery; ordering is intentionally not deterministic.
+
+2. **`walkdir` remains the core baseline implementation pattern**: a highly optimized single-thread DFS with explicit stack state (`stack_list`, `stack_path`), open-handle throttling (`max_open`), lazy-ish metadata strategy, and robust error continuation. It trades raw multicore traversal parallelism for predictable semantics and low overhead.
+
+3. **`ignore` is not just “walkdir + gitignore”**; it is a full traversal engine with:
+   - dynamic per-directory ignore matcher composition,
+   - explicit precedence model (`overrides` → ignore files → types → hidden/filesize/filter),
+   - both single-thread iterator and multithread work-stealing traversal modes.
+
+4. **`scandir-rs` is architecturally a wrapper product over a parallel walker (`jwalk-meta`) plus channelized background execution**, with additional metadata-heavy result shaping and Python bindings. Its “parallelism strategy” for traversal is mostly inherited; its own concurrency is primarily producer-thread + channel + consumer API.
+
+5. **`dowser` (current code) is not multithreaded** despite the objective text describing it as such. It is a single-thread recursive crawler that strongly emphasizes **canonicalization + hash-based dedup** and symlink-follow behavior. Dedup is on canonical path hash, not inode identity.
+
+---
+
+## 1) jwalk deep dive (Byron, Rayon-based parallel walk)
+
+### 1.1 Core parallel architecture: ordered job queues + Rayon `par_bridge`
+
+`jwalk`’s internal design splits traversal into two layers:
+- **ReadDirSpec production/consumption** (units of “read this directory” work)
+- **DirEntry streaming** (yielding user-visible entries in recursive order)
+
+The key parallel entry point is in `src/core/read_dir_iter.rs`:
+
+```rust
+read_dir_spec_iter.par_bridge().for_each_with(
+    run_context,
+    |run_context, ordered_read_dir_spec| {
+        multi_threaded_walk_dir(ordered_read_dir_spec, run_context);
+    },
+);
+```
+
+(From `/tmp/jwalk/src/core/read_dir_iter.rs`)
+
+This is significant:
+- It does **not** build traversal from Rayon `scope` trees.
+- It converts an internal iterator into a parallel stream via `par_bridge`, then uses threadpool workers to consume `ReadDirSpec` tasks.
+
+Work distribution is driven by Rayon scheduling of `par_bridge`, with `ReadDirSpec` items fed from an internal queue.
+
+### 1.2 Work scheduling and ordering model
+
+`jwalk` has two queues (`src/core/read_dir_iter.rs`):
+- `read_dir_spec_queue` with **Relaxed** ordering: feed workers quickly.
+- `read_dir_result_queue` with **Strict** ordering: emit results in DFS order.
+
+Queue implementation (`src/core/ordered_queue.rs`) uses:
+- `crossbeam::channel::unbounded` transport,
+- `BinaryHeap<Ordered<T>>` as receive buffer,
+- `OrderedMatcher` with `IndexPath` and child-count stack to enforce strict recursive ordering.
+
+Critical mechanism:
+- Every work/result gets an `IndexPath` (e.g., `[0, 3, 2]`) representing location in traversal tree.
+- Strict result iterator only releases next exact expected index-path (`looking_for`), buffering out-of-order completed work until its turn.
+
+This is effectively a **parallel execution + deterministic replay** architecture.
+
+### 1.3 Streaming results
+
+`jwalk` results are streamed via iterator stack architecture:
+- `ReadDirIter::ParWalk` yields `ReadDir` results as they arrive in strict order.
+- `DirEntryIter` consumes these `ReadDir` batches and yields `DirEntry` values incrementally.
+
+`DirEntryIter::next` can therefore return entries while deeper traversal is still executing in worker threads. So yes: **consumer can consume while walk is in progress**.
+
+It is **iterator-based streaming backed by channels/queues**, not callback-only.
+
+### 1.4 Sorting and performance impact
+
+Sorting is per-directory (`sort(true)`), implemented as:
+
+```rust
+dir_entry_results.sort_by(|a, b| match (a, b) {
+    (Ok(a), Ok(b)) => a.file_name.cmp(&b.file_name),
+    ...
+});
+```
+
+(from `src/lib.rs`)
+
+Implications:
+- Adds `O(k log k)` per directory with `k` children.
+- Increases latency for yielding first child in each directory (must gather all children first).
+- Improves deterministic output and downstream cache/locality for some workloads.
+
+Because jwalk parallelizes at directory granularity, sorting does not remove parallelism globally, but it reduces per-directory streaming granularity.
+
+### 1.5 Client state pattern
+
+One of jwalk’s strongest internals is explicit **branch state propagation**:
+- `ReadDirState` is attached to `ReadDirSpec` (`client_read_state`), cloned and passed downward.
+- Callback `process_read_dir(depth, path, &mut read_dir_state, &mut children)` can mutate state and per-entry `client_state`.
+
+In `ReadDir::read_children_specs`, child specs inherit either parent-cloned state or overridden state from `ReadChildren.client_read_state`.
+
+This pattern allows implementing hierarchical filters (e.g., per-subtree policy, ignore context, depth-dependent toggles) without global mutable shared state.
+
+### 1.6 Error handling and symlink behavior
+
+Error model (`src/core/error.rs`) includes:
+- IO errors with optional path + depth,
+- loop detection errors (`ancestor`, `child`),
+- threadpool busy/deadlock-protection sentinel (`ThreadpoolBusy`).
+
+`follow_links`:
+- optional globally,
+- root symlink is special-cased to preserve semantics while still descending when root points to dir,
+- loop checks compare symlink target against ancestor chain tracked in `follow_link_ancestors`.
+
+Permission-denied, broken links, and read failures are surfaced as iterator `Err(...)` values; traversal otherwise continues.
+
+### 1.7 Platform abstraction + metadata availability
+
+In this crate version, `jwalk` itself has **very little explicit platform cfg branching** in core files (mostly standard-library APIs).
+
+Metadata:
+- `DirEntry` stores `file_name`, `file_type`, depth, parent path, and traversal controls.
+- It does **not** eagerly attach full stat metadata by default; `metadata()` performs `symlink_metadata`/`metadata` on demand.
+- So available without extra stat: mainly `file_type` (from `fs::DirEntry::file_type`) + names/paths/depth.
+
+---
+
+## 2) walkdir deep dive (BurntSushi, single-thread baseline)
+
+### 2.1 DFS implementation and explicit stack state
+
+Core iterator (`src/lib.rs`, `IntoIter`) keeps:
+- `stack_list: Vec<DirList>` (open or closed directory streams),
+- `stack_path: Vec<Ancestor>` for loop checks when following symlinks,
+- `oldest_opened` index for max-open-fd policy,
+- `deferred_dirs` for `contents_first` mode.
+
+Traversal loop in `next()`:
+1. Handle initial root.
+2. While stack non-empty:
+   - maintain depth,
+   - enforce max depth,
+   - advance top `DirList`,
+   - handle each entry through `handle_entry`.
+
+This is a classic iterative DFS with explicit control over file-descriptor pressure.
+
+### 2.2 `follow_links`, loop checks, root-link behavior
+
+`handle_entry` + `follow` + `check_loop` implement symlink logic:
+- if `follow_links` and entry is symlink, reify entry from followed path (`from_path(..., follow=true)`),
+- for followed dir links, detect cycles by comparing candidate handle to ancestor handles,
+- root symlink gets special handling (`follow_root_links`) similar in spirit to jwalk: can descend while preserving reporting semantics.
+
+Loop detection uses `same_file::Handle` comparison, platform-tailored (`Ancestor::is_same` differs on Windows vs non-Windows).
+
+### 2.3 Same-device filtering
+
+When `same_file_system(true)`:
+- root device captured at start (`util::device_num`),
+- each candidate directory checked via `is_same_file_system` before descending.
+
+Platform implementations:
+- Unix: `MetadataExt::dev()`
+- Windows: volume serial number via `winapi_util`
+
+### 2.4 Metadata and `d_type`/stat behavior
+
+`walkdir::DirEntry` stores file type (`ty`) and (on Unix) inode from dirent; this avoids repeated metadata calls for common checks.
+
+Important behavior:
+- `file_type()` is syscall-free after entry creation.
+- `metadata()` is on-demand (`symlink_metadata`/`metadata` depending on `follow_link`).
+- `from_entry` uses `ent.file_type()` and on Unix gets inode via `DirEntryExt::ino()`.
+
+In practice, `ent.file_type()` can leverage directory-entry type (`d_type`) where OS/filesystem provides it, avoiding full `stat` in common paths.
+
+### 2.5 Continue-on-error semantics
+
+Errors are values in iteration stream (`Result<DirEntry, Error>`). `DirList::Opened` stores initial `read_dir` open error in an `Option` so it can be emitted once and traversal can continue where possible.
+
+Error struct carries depth + path + loop context, making recovery/diagnostics straightforward.
+
+### 2.6 Performance optimizations in design
+
+Key internal optimization choices:
+- **FD throttling with `max_open`**: closes old directory handles into buffered vectors (`DirList::Closed`) when needed.
+- **Avoid unnecessary allocations in paths** by keeping `DirEntry` path and borrowing where possible.
+- **Optional sorting** moves directory entries into memory once then sorts.
+- **Cheap file-type checks** stored on entry.
+
+### 2.7 Why single-threaded?
+
+In this codebase, there is no explicit in-source “anti-parallelism manifesto.” The practical interpretation is:
+- `walkdir` is a focused, deterministic, low-overhead iterator primitive.
+- BurntSushi’s ecosystem uses `ignore` for parallel traversal + ignore logic where needed.
+
+So ecosystem-wise: **single-thread traversal primitive (`walkdir`) + higher-level parallel engine (`ignore`)** rather than parallelizing `walkdir` itself.
+
+---
+
+## 3) ignore crate deep dive (ripgrep’s traversal engine)
+
+## 3.1 Two walkers: single-thread `Walk` and parallel `WalkParallel`
+
+`WalkBuilder::build()` creates single-thread walk over `walkdir`, adding ignore semantics.  
+`WalkBuilder::build_parallel()` creates `WalkParallel`, which is a separate execution engine.
+
+Parallel API is visitor-driven, not iterator-driven:
+
+```rust
+builder.build_parallel().run(|| {
+    Box::new(|entry| { ...; WalkState::Continue })
+})
+```
+
+This avoids forcing ordered pull-based semantics across threads.
+
+### 3.2 Parallel engine internals (`WalkParallel`)
+
+`WalkParallel::visit`:
+- creates initial `Work` messages for root paths,
+- determines thread count (`available_parallelism().min(12)` when threads=0),
+- builds per-thread work-stealing stacks,
+- spawns scoped worker threads.
+
+Work payload:
+- `dent` (dir entry),
+- cloned `Ignore` matcher context,
+- optional root device.
+
+### 3.3 Work distribution and work-stealing strategy
+
+`Stack` wraps `crossbeam_deque::Worker<Message>` with `new_lifo()`. Why LIFO?
+
+Code comment explicitly says DFS-oriented LIFO avoids disastrous memory behavior on wide trees with many gitignores.
+
+Stealing logic:
+- each worker pops local first,
+- if empty, steals batch+pop from other workers in fair rotated order.
+
+This is a strong throughput design: locality from LIFO + load balancing from stealing.
+
+### 3.4 Synchronization and shutdown protocol
+
+No central blocking channel. Synchronization uses:
+- local deques + stealing,
+- `quit_now: AtomicBool` for global early termination,
+- `active_workers: AtomicUsize` to detect global quiescence.
+
+When a worker sees no work:
+- decrements `active_workers`,
+- if it reaches 0, emits `Quit` and terminates search globally,
+- otherwise sleeps briefly (1ms) and rechecks.
+
+This provides lock-light scheduling with termination detection.
+
+### 3.5 Visitor semantics and result flow
+
+`ParallelVisitor` runs on each worker thread; results are pushed directly to user visitor callback in that worker.
+
+Consequences:
+- no global ordering guarantees,
+- no central merge queue bottleneck,
+- user responsible for synchronization if aggregating shared state.
+
+This is different from jwalk’s ordered stream philosophy.
+
+### 3.6 Gitignore integration architecture
+
+Ignore matching is built around `Ignore`/`IgnoreInner` tree (`dir.rs`), where each directory context carries:
+- custom ignore matcher,
+- `.ignore` matcher,
+- `.gitignore` matcher,
+- `.git/info/exclude` matcher,
+- global gitignore matcher,
+- overrides and type matcher handles,
+- parent link.
+
+`add_child` compiles child-level matchers; `add_parents` can prebuild parent chain for roots.
+
+Performance-focused note: `create_gitignore` checks `gipath.exists()` (except windows path) before opening files, based on expectation that most dirs have no ignore file.
+
+### 3.7 Matching order and filtering-before-stat
+
+Documented precedence in `WalkBuilder` docs and implemented in `Ignore::matched`:
+1. overrides,
+2. ignore files (`.ignore`, `.gitignore`, excludes, global, explicit),
+3. type matcher,
+4. hidden handling,
+5. filesize/filter checks later in walker.
+
+Critical optimization in single-thread `Walk::skip_entry` comment: do cheap skip checks before expensive stat/filesystem ops.
+
+In parallel path, `generate_work` calls `should_skip_entry(ig, dent)` before filesize stat and before queueing work. So filtering is largely pre-stat when possible.
+
+### 3.8 Type-based filtering
+
+`types.rs` compiles selected type globs into a `GlobSet` + mapping from glob index to selection definition. Matching uses file name only (not dirs) and returns whitelist/ignore with precedence by last match.
+
+`Pool<Vec<usize>>` is used as reusable per-thread temporary match storage to reduce allocations.
+
+### 3.9 Overhead vs raw walkdir
+
+`ignore` overhead sources:
+- matcher construction per directory,
+- ignore file I/O/parsing,
+- globset match checks.
+
+But it gains:
+- major pruning on ignored trees,
+- optional parallel scaling,
+- practical end-to-end wins for ripgrep-like workloads where pruning dominates.
+
+---
+
+## 4) scandir-rs deep dive (brmmm3)
+
+### 4.1 Architecture and relation to jwalk
+
+`scandir-rs` core crates (`scandir`, `pyscandir`) depend on `jwalk-meta` and use `WalkDirGeneric` directly:
+
+```rust
+for result in WalkDirGeneric::new(&options.root_path)
+    .skip_hidden(...)
+    .follow_links(...)
+    .sort(...)
+    .max_depth(...)
+    .read_metadata(true)
+    .read_metadata_ext(...)
+    .process_read_dir(...)
+```
+
+So traversal internals are mostly inherited from `jwalk-meta`; `scandir-rs` adds:
+- filter plumbing,
+- channelized result transport (`flume`),
+- typed output shaping (`Toc`, `ScandirResult`, statistics),
+- background thread control APIs.
+
+### 4.2 Parallelism strategy
+
+There are two levels:
+1. **Internal tree walk parallelism from `jwalk-meta`** (Rayon-based, inherited behavior).
+2. **API-level background thread** (`thread::spawn`) that runs scanning and sends results through `flume` channels.
+
+So from caller POV it is asynchronous streaming. But its own crate-level worker model is mostly **single producer thread** wrapping a parallel walker backend.
+
+### 4.3 Stat/metadata collection behavior
+
+For `Scandir`/`Count`, it explicitly enables metadata collection:
+- `.read_metadata(true)`
+- `.read_metadata_ext(return_type == Ext)`
+
+This means richer per-entry stat is gathered during traversal. It is not a separate explicit “batch stat queue”; rather metadata capture is integrated in walker pipeline.
+
+### 4.4 Filtering model
+
+`common.rs` compiles glob filters (include/exclude for dirs/files), then `filter_children` mutates the child vector inside `process_read_dir` callback. This can prune recursion by dropping directory children early.
+
+### 4.5 Python bindings (FFI structure)
+
+`pyscandir` uses **PyO3** classes wrapping Rust structs (`Count`, `Walk`, `Scandir`).
+
+Pattern:
+- Python object owns Rust scanner instance.
+- `start()` spawns internal Rust background thread.
+- Python `__next__` polls `results(true)` and sleeps 10ms when empty while busy.
+- conversion wrappers map Rust structs into Python `pyclass` wrappers.
+
+This is a relatively clean object-oriented FFI boundary with direct method forwarding + type conversion.
+
+### 4.6 Unique traits / caveats
+
+Unique:
+- integrated Rust+Python API surface,
+- richer stat payload (`DirEntryExt`) including inode/dev/link counts etc,
+- optional serialization outputs (`speedy`, `bincode`, `json`).
+
+Caveat found in code: `Walk::has_errors` currently returns `!self.has_errors`, which appears inverted.
+
+---
+
+## 5) dowser deep dive (Blobfolio)
+
+### 5.1 Actual thread model
+
+Current `dowser` source has **no multithread traversal**. The iterator is single-threaded:
+- `files` and `dirs` vectors as internal stacks,
+- `Iterator::next` pops files, otherwise pops a directory and `read_dir`s it.
+
+So any “multi-threaded recursive finder” characterization does not match current implementation.
+
+### 5.2 Symlink following and canonicalization
+
+`Entry::from_path` and `Entry::from_dir_entry` implement path typing:
+- if symlink following disabled, symlinks are dropped immediately (`symlink_metadata` check).
+- if following enabled, symlink path is canonicalized and classified from canonical target metadata.
+
+All returned entries are canonicalized, which naturally collapses many link aliases.
+
+### 5.3 Dedup strategy
+
+Dedup is via `seen: HashSet<u64>` where key is hash of canonical path bytes (unix optimized) using fixed-seed `ahash`.
+
+Not inode-based dedup:
+- two hard links to same inode but different canonical paths can remain distinct (unless canonicalization collapses path identity, which it usually does not for hard links).
+- dedup target is **path identity after canonicalization**, not file identity.
+
+### 5.4 Error handling model
+
+Traversal is best-effort and mostly silent on IO errors during crawl (`read_dir` errors and entry conversion failures are skipped/continued). Some API methods return explicit errors (e.g., reading list files).
+
+This is intentionally simple and throughput-oriented but offers less diagnostic richness than walkdir/ignore/jwalk error streams.
+
+---
+
+## 6) Cross-cutting comparison
+
+| Aspect | jwalk | walkdir | ignore | scandir-rs | dowser |
+|---|---|---|---|---|---|
+| Parallel | Yes (Rayon backend) | No | Yes (custom work-stealing) | Yes (via `jwalk-meta` backend + async producer thread) | No (current source single-thread) |
+| Thread count | Configurable (`Serial`, default pool, existing pool, new pool n) | 1 | Configurable; default auto `available_parallelism().min(12)` | Internal walker backend-config dependent; wrapper uses 1 producer thread | 1 |
+| Work distribution | Queue of `ReadDirSpec` + Rayon `par_bridge`; ordered results | N/A | Per-thread LIFO deques + steal from peers (`crossbeam_deque`) | Backend walker does traversal; crate wraps with channel streaming | Local vectors (DFS-like) |
+| Streaming | Iterator yields while workers run; ordered streaming | Iterator | Parallel visitor callbacks per-thread (unordered) | Channel-based incremental retrieval (`flume`) | Iterator |
+| Sorting | Optional per-directory sort | Optional sort_by | Sort in single-thread mode through walkdir; no global ordered output in parallel mode | Optional (`sorted`) passed through backend | No explicit sort |
+| `d_type` usage | Via std `DirEntry::file_type()` (indirect) | Yes, via std `DirEntry::file_type()` path and stored file type | In single-thread path uses walkdir; raw parallel path also captures file_type from dir entries | Via `jwalk-meta` dir-entry handling | Uses `DirEntry::file_type()` |
+| Hard link dedup | No built-in | No | Not built-in dedup | Partial in statistics (tracks inode to reduce hardlink double count) | Path-hash dedup, not inode dedup |
+| Symlink handling | Configurable, loop detection via ancestor path chain | Configurable, loop detection via handle equality | Configurable, loop detection using parent ignore ancestry handles | Configurable (`follow_links`) delegated to backend | Follow-by-default with canonicalization; optional disable |
+| Platform-specific paths | Minimal explicit cfg in this crate | Strong cfg branches (unix/windows/other) | Strong cfg branches + git worktree handling | Delegates much to backend + own cfg for metadata fields | Unix-focused crate, path-hash optimization |
+| Error handling | Iterator `Result`, includes threadpool-busy/loop/io with depth/path | Iterator `Result`, continue-on-error + loop context | Visitor gets `Result`; partial ignore errors attached; continue unless `Quit` | Errors delivered in result channels/lists; mixed quality | Mostly skip/continue during crawl |
+
+---
+
+## 7) API design comparison for disk-scanner use case
+
+### Most ergonomic for “scanner core + pluggable policy + deterministic output”
+
+- **jwalk** has the most direct shape for this: iterator API + per-directory callback + propagated client state + optional sorting + parallelism controls.
+- It naturally supports “scan and aggregate” pipelines that prefer deterministic tree order and composable stateful filtering.
+
+### Most ergonomic for “search tooling semantics (gitignore/types/overrides)”
+
+- **ignore** is strongest if we need ripgrep-like semantics exactly.
+- But parallel mode API is callback/visitor style, which is less ergonomic than iterator for some consumers.
+
+### Best low-level primitive baseline
+
+- **walkdir** is still the cleanest low-level serial iterator primitive; ideal for correctness-first fallback paths or environments where parallel traversal is not beneficial.
+
+### Most productized app-layer wrapper
+
+- **scandir-rs** is a productized scanner package (channels, progress, Python wrappers, result bundles). Good reference for API shape, less useful as traversal-engine reference because heavy logic is in dependency.
+
+### Simplest canonical-dedup crawler
+
+- **dowser** is tiny and easy to reason about; useful as a reference for canonical path dedup behavior and “just iterate files” minimalism.
+
+---
+
+## 8) Performance implications of design choices
+
+1. **Ordered parallelism (`jwalk`)**  
+   Pros: deterministic order, iterator compatibility.  
+   Cost: strict-order buffering and matcher bookkeeping may delay fast-completed subtrees waiting on earlier siblings.
+
+2. **Unordered work-stealing (`ignore::WalkParallel`)**  
+   Pros: high throughput and better multicore utilization under uneven trees; less global coordination.  
+   Cost: nondeterministic result order; callback model complexity.
+
+3. **Per-directory matcher compilation (`ignore`)**  
+   Pros: precise gitignore semantics and strong pruning.  
+   Cost: additional matcher management overhead; more memory for matcher context graph.
+
+4. **Single-thread explicit stack (`walkdir`)**  
+   Pros: very predictable behavior, low synchronization overhead, strong baseline performance.  
+   Cost: no CPU-level traversal parallel scaling.
+
+5. **Metadata-rich traversal (`scandir-rs`)**  
+   Pros: no secondary stat pass for rich result payloads.  
+   Cost: more syscall/load per entry when extended metadata requested.
+
+6. **Canonical path dedup (`dowser`)**  
+   Pros: robust symlink-alias collapse, simple cycle avoidance side effect.  
+   Cost: canonicalization can be expensive; dedup does not equal inode dedup semantics.
+
+---
+
+## 9) Recommendation for our disk scanner
+
+### Short recommendation
+
+**Build on `jwalk`-style architecture for core traversal, and selectively borrow `ignore` matcher architecture where needed.**
+
+### Why not “just use ignore as-is”?
+
+Use `ignore` as-is if our scanner must exactly replicate gitignore/type/override semantics and can accept visitor-style parallel output.
+
+But if our scanner needs:
+- deterministic ordered streaming,
+- richer per-directory state passing,
+- simpler iterator consumption model,
+then `jwalk`-style core is a better fit.
+
+### Why not fork walkdir directly?
+
+You would need to add substantial parallel orchestration and ordered merge logic anyway. At that point, you are effectively rebuilding parts of jwalk/ignore architecture.
+
+### Why not adopt scandir-rs core?
+
+Its traversal engine is already delegated to `jwalk-meta`; value is mostly wrapper APIs and Python integration patterns. Good inspiration, but not ideal as sole foundation unless we also want that exact API package style.
+
+### Why not dowser?
+
+Too minimal for scanner-grade observability/control, no parallel traversal, and dedup semantics are path-hash-centric.
+
+---
+
+## 10) If building custom: patterns to steal from each library
+
+### From jwalk
+- Ordered work/result queue split (`Relaxed` work queue + `Strict` result queue).
+- Index-path based deterministic replay.
+- Propagated per-branch client state (`ReadDirState`) and per-entry client state hooks.
+- Busy/deadlock guard when using shared threadpools.
+
+### From walkdir
+- Battle-tested DFS stack model and depth bookkeeping.
+- `max_open` file-descriptor pressure control.
+- Robust error type carrying depth/path/loop metadata.
+- Cross-platform same-device and loop-handle logic.
+
+### From ignore
+- Work-stealing LIFO deques with fair steal policy.
+- Structured ignore precedence model and matcher layering.
+- Fast-path skip checks before expensive stats.
+- Per-thread visitor model for lock-light aggregation.
+
+### From scandir-rs
+- Channelized background producer API for async consumption.
+- Rich typed result payload design (`base` vs `ext` metadata levels).
+- Python binding layering via PyO3 for future FFI targets.
+
+### From dowser
+- Canonicalization-first dedup for path-identity semantics.
+- Very small-state iterator core for simple fallback mode.
+
+---
+
+## 11) Key takeaways for our disk scanner
+
+1. **Decide early whether deterministic traversal order is a hard requirement.**  
+   If yes, use jwalk-like ordered merge. If no, use ignore-like work-stealing callbacks for throughput.
+
+2. **Separate traversal from filtering policy.**  
+   Keep walker core independent; plug in matcher layers (gitignore/types/hidden/size) similarly to ignore.
+
+3. **Keep stat calls late and optional.**  
+   Emulate ignore/walkdir fast-path checks to avoid expensive metadata reads unless needed.
+
+4. **Model depth, symlink, and same-device as first-class controls.**  
+   All mature libraries expose these because they dominate correctness and surprise behavior.
+
+5. **Use explicit error streams rather than fail-fast global abort.**  
+   Continue-on-error is essential for real-world filesystem scans.
+
+6. **If we need cross-language SDKs, copy scandir-rs binding shape, not traversal internals.**
+
+7. **For dedup semantics, distinguish clearly between path dedup and inode dedup.**  
+   They solve different problems and should likely be separate toggles.
+
+---
+
+## Appendix: representative internal snippets by library
+
+### jwalk: parallel read-dir with strict ordered result queue
+
+```rust
+let read_dir_result_queue = new_ordered_queue(stop.clone(), Ordering::Strict);
+let read_dir_spec_queue = new_ordered_queue(stop.clone(), Ordering::Relaxed);
+...
+read_dir_spec_iter.par_bridge().for_each_with(run_context, |ctx, spec| {
+    multi_threaded_walk_dir(spec, ctx);
+});
+```
+
+### walkdir: stack-driven DFS core loop
+
+```rust
+while !self.stack_list.is_empty() {
+    self.depth = self.stack_list.len();
+    if self.depth > self.opts.max_depth { self.pop(); continue; }
+    let next = self.stack_list.last_mut().unwrap().next();
+    match next {
+        None => self.pop(),
+        Some(Err(err)) => return Some(Err(err)),
+        Some(Ok(dent)) => if let Some(r) = self.handle_entry(dent) { return Some(r); },
+    }
+}
+```
+
+### ignore: work-stealing LIFO stacks
+
+```rust
+let deques: Vec<Deque<Message>> =
+    std::iter::repeat_with(Deque::new_lifo).take(threads).collect();
+...
+fn pop(&self) -> Option<Message> {
+    self.deque.pop().or_else(|| self.steal())
+}
+```
+
+### ignore: parallel worker execution
+
+```rust
+while let Some(work) = self.get_work() {
+    if let WalkState::Quit = self.run_one(work) {
+        self.quit_now();
+    }
+}
+```
+
+### scandir-rs: wrapper over jwalk-meta + channel streaming
+
+```rust
+self.thr = Some(thread::spawn(move || {
+    let start_time = Instant::now();
+    entries_thread(options, filter, tx, stop);
+    *duration.lock().unwrap() = start_time.elapsed().as_secs_f64();
+    finished.store(true, Ordering::Relaxed);
+}));
+```
+
+### dowser: canonicalize and hash-dedup
+
+```rust
+if let Ok(path) = std::fs::canonicalize(path)
+    && let Ok(meta) = std::fs::symlink_metadata(&path)
+{
+    if meta.is_dir() { Some(Self::Dir(path)) } else { Some(Self::File(path)) }
+} else { None }
+...
+if self.seen.insert(e.hash()) {
+    match e { Entry::Dir(p) => self.dirs.push(p), Entry::File(p) => self.files.push(p) }
+}
+```