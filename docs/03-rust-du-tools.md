# Rust Disk Usage Tooling Research: diskus vs dua-cli vs dust vs volscan vs dirscan vs dutree

## Executive summary

1. **There are two dominant high-performance patterns in this ecosystem:**
   - *Parallel streaming walk + incremental aggregation* (`diskus`, `dua-cli` aggregate mode, `volscan`, `dirscan`)
   - *Parallel walk + full in-memory tree materialization* (`dua-cli` interactive, `dust`, `dutree`)
   If we optimize for very large filesystems, the first pattern wins on memory predictability.

2. **`jwalk`-based scanners (`dua-cli`, `volscan`, `dirscan`) externalize parallel directory traversal to a specialized walker**, while `diskus` and `dust` implement more custom traversal orchestration (Rayon + recursion / channel reduction). `jwalk` gives robust parallel traversal primitives and ordered depth output; custom walkers give tighter control but increase complexity.

3. **Stat/syscall strategy is a major differentiator:**
   - `diskus` and `dust` use `symlink_metadata` + Unix `MetadataExt::blocks()` style accounting for disk usage.
   - `dua-cli`, `volscan`, `dirscan` use `filesize::PathExt::size_on_disk_fast()` where needed.
   - `dust` has the most elaborate cross-platform sizing path (notably Windows fast/expensive branches and Unix NTFS over-allocation guard).

4. **Hard-link handling varies significantly and affects correctness:**
   - Strong dedup: `diskus` (`HashSet<(dev, inode)>` for files with `nlink > 1`), `dua-cli` (`InodeFilter` with remaining-link tracking), `dust` (`clean_inodes()` pass unless apparent size mode).
   - No explicit dedup: `volscan`, `dirscan`, `dutree`.

5. **For our own scanner, best-of-breed architecture is: `jwalk` parallel walk + explicit device/inode policies + streaming aggregation + optional bounded tree index for UI.** We should avoid unbounded full-tree retention by default, and make memory-heavy views opt-in.

---

## Scope and method

I inspected the hot path (walking + metadata + aggregation) in the six repositories:

- `sharkdp/diskus`
- `Byron/dua-cli`
- `bootandy/dust`
- `trinhminhtriet/volscan`
- `orf/dirscan`
- `nachoparker/dutree`

Emphasis is on concurrency model, syscall strategy, in-memory representation, and resulting performance properties. CLI polish/UI rendering is only covered where it affects the scanning architecture.

---

## 1) diskus deep dive

`diskus` is intentionally small and focused: sum usage quickly with minimal policy surface.

### Parallelism model

Core traversal is recursive and parallelized with Rayon over each directory level:

```rust
fn walk(tx: channel::Sender<Message>, entries: &[PathBuf], filesize_type: CountType, directories: Directories) {
    entries.into_par_iter().for_each_with(tx, |tx_ref, entry| {
        if let Ok(metadata) = entry.symlink_metadata() {
            ...
            if should_count {
                let unique_id = generate_unique_id(&metadata);
                let size = filesize_type.size(&metadata);
                tx_ref.send(Message::SizeEntry(unique_id, size)).unwrap();
            }
            if is_dir {
                let mut children = vec![];
                match fs::read_dir(entry) { ... }
                walk(tx_ref.clone(), &children[..], filesize_type, directories);
            }
        }
    });
}
```

This model is paired with a **separate receiver thread** that drains a channel and performs reduction (total size + error list + hard-link dedup set).

### Thread count strategy

`DiskUsage::default_num_workers()` chooses:

- `3 * available_parallelism`
- capped at `64`

The code comments explicitly call this a cold-cache/warm-cache tradeoff (higher parallelism helps I/O scheduling when cold, but synchronization overhead hurts warm cache).

### Metadata/syscall strategy

- Uses `symlink_metadata()` (lstat semantics, no symlink following for metadata fetch itself).
- Apparent size: `metadata.len()`
- Disk usage on Unix: `metadata.blocks() * 512`

That maps to standard kernel metadata syscalls via `std::fs` abstraction.

### Hard links and cross-device policy

Hard-link dedup is explicit:

```rust
if metadata.is_file() && metadata.nlink() > 1 {
    Some(UniqueID { device: metadata.dev(), inode: metadata.ino() })
}
```

The reducer tracks a `HashSet<UniqueID>` and only accumulates unseen IDs.

Cross-device traversal is **not filtered**; traversal follows whatever subtree is reachable from provided roots (except symlink recursion behavior inherited from directory walk logic and read_dir semantics).

### Memory behavior

- Streaming reductions through channel keep aggregation state compact (`u64 total`, `Vec<Error>`, `HashSet<UniqueID>`).
- However, each directory visit allocates a `Vec<PathBuf>` for children before recursive call.
- Memory footprint is generally low unless many hard links (large ID set) or high error cardinality.

### Performance implications

`diskus` is efficient for one scalar output because it does no tree retention and minimal per-entry processing. The channel + reducer split avoids lock contention in worker tasks. The recursive call pattern is simple but could become stack-sensitive on pathological depth if not constrained by filesystem reality.

---

## 2) dua-cli deep dive

`dua-cli` has two distinct hot paths: **aggregate mode** and **interactive mode**.

### 2.1 Walk integration with jwalk

`WalkOptions::iter_from_path()` builds a configured `jwalk::WalkDirGeneric`:

```rust
WalkDir::new(root)
    .follow_links(false)
    .min_depth(if skip_root { 1 } else { 0 })
    .sort(...)
    .process_read_dir({ ... attach metadata, device boundary checks, ignore_dirs ... })
    .parallelism(match self.threads {
        0 => jwalk::Parallelism::RayonDefaultPool { ... },
        1 => jwalk::Parallelism::Serial,
        _ => jwalk::Parallelism::RayonExistingPool {
            pool: jwalk::rayon::ThreadPoolBuilder::new()
                .stack_size(128 * 1024)
                .num_threads(self.threads)
                .thread_name(|idx| format!(\"dua-fs-walk-{idx}\"))
                .build()?
                .into(),
            busy_timeout: None,
        },
    })
```

Notable details:

- metadata is fetched in `process_read_dir` and stashed into `client_state`, reducing repeated metadata calls downstream.
- optional same-device gating is applied at directory expansion time (`read_children_path = None`) and again when counting.

### 2.2 Thread count strategy

CLI default is platform-tuned:

- macOS: default `3`
- others: default `0` meaning “auto”
- then main resolves `0 -> num_cpus::get()`

So practical default is `NCPU` (except explicit macOS special-case).

### 2.3 Aggregate mode pipeline (streaming)

`aggregate.rs` is mostly single-pass streaming accumulation over jwalk output:

```rust
for entry in walk_options.iter_from_path(path.as_ref(), device_id, false) {
    match entry {
        Ok(entry) => {
            let file_size = match entry.client_state {
                Some(Ok(ref m)) if (count_hard_links || inodes.add(m)) && same_device => {
                    if apparent_size { m.len() }
                    else { entry.path().size_on_disk_fast(m).unwrap_or_else(|_| 0) }
                }
                _ => 0,
            };
            num_bytes += file_size as u128;
        }
        Err(_) => num_errors += 1,
    }
}
```

This path is memory-cheap and close to “du + robust options”.

### 2.4 Interactive mode pipeline (walk → integrate → UI)

Interactive mode introduces a background dispatcher thread that streams `TraversalEvent`s over a bounded crossbeam channel (capacity 100):

```rust
for entry in walk_options.iter_from_path(...).into_iter() {
    entry_tx.send(TraversalEvent::Entry(entry, Arc::clone(&root_path), device_id))?
}
entry_tx.send(TraversalEvent::Finished(io_errors))?
```

The UI event loop `select!`s between terminal events and traversal events, integrating incrementally:

```rust
recv(&active_traversal.event_rx) -> event => {
    if let Some(is_finished) = active_traversal.integrate_traversal_event(traversal, event) {
        ... refresh state/UI ...
    }
}
```

This is a strong architecture for responsiveness: bounded buffering + incremental tree updates + throttled redraws.

### 2.5 Directory size tracking data structure

Interactive aggregation uses a `petgraph::StableGraph<EntryData, ()>` plus depth-state stacks:

- `directory_info_per_depth_level: Vec<EntryInfo>`
- `parent_node_size_per_depth_level: Vec<u128>`
- mutable rolling `current_directory_at_depth`

As depth changes, it pushes/pops and finalizes parent nodes without repeatedly re-traversing subtrees. This is effectively a streaming fold into a graph.

`EntryData` packs: name, size, mtime, entry_count, metadata error bit, dir bit.

### 2.6 Hard links, cross-device, metadata strategy

- Hard links: `InodeFilter` (`HashMap<(dev, inode), remaining_links>`) tracks whether an inode should still count.
- Cross-device: `crossdev::init(path)->device_id`, then `is_same_device(device_id, metadata)` checks at prune and/or count points.
- Disk usage syscall path: `filesize::PathExt::size_on_disk_fast(meta)` (`size_on_disk()` variant on Windows path join case).

### 2.7 Safe deletion semantics

Deletion in TUI is explicit, iterative, and symlink-safe:

```rust
let is_symlink = path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(true);
if is_symlink { fs::remove_file(&path); continue; }
...
for dir in dirs.into_iter().rev() {
    fs::remove_dir(&dir).or_else(|_| fs::remove_file(dir));
}
```

This avoids symlink traversal during recursive delete and updates in-memory tree + recomputed parent sizes after mutation.

### Performance characteristics

- Aggregate mode: high throughput, low memory.
- Interactive mode: higher memory (README cites about ~60MB per 1M entries) due to full graph + UI state, but enables rich navigation/mutation.
- jwalk integration plus pre-attached metadata is very good for balancing speed and modularity.

---

## 3) dust deep dive

`dust` optimizes for tree visualization while still being fast.

### Parallelism model

Core walk recursively uses `std::fs::read_dir` + Rayon `par_bridge()`:

```rust
entries
    .into_iter()
    .par_bridge()
    .filter_map(|entry| {
        if !ignore_file(entry, walk_data) && let Ok(data) = entry.file_type() {
            if data.is_dir() || (follow_links && data.is_symlink()) {
                return walk(entry.path(), walk_data, depth + 1);
            }
            return build_node(...);
        }
        None
    })
    .collect()
```

This parallelizes per-directory iterator consumption while recursively descending.

### Thread/stack strategy

`dust` explicitly initializes a Rayon pool and can set:

- thread count (`--threads`)
- large stack size (`--stack-size` or auto up to 1 GiB/thread when memory permits)

The large-stack heuristic is specifically to avoid stack overflows on deep directory structures.

### Metadata/syscall strategy (very detailed cross-platform path)

`platform::get_metadata()` encapsulates platform behavior.

Unix branch:
- `symlink_metadata` vs `metadata` depending on follow-links
- apparent: `len()`
- usage: `blocks()*512` with guard against suspicious over-reporting (NTFS mount behavior) by capping using `blksize` + preallocation buffer heuristic.

Windows branch:
- optimized fast path avoiding expensive full file opens for common files
- fallback “expensive” path via limited attribute handle + `winapi_util::file::information`
- extensive comments explain AV/Windows Defender and syscall cost behavior.

This is the most system-aware metadata layer among the six tools.

### Tree construction and aggregation

`Node` is recursive (`name`, `size`, `children`, optional inode/device, `depth`).

After initial tree build, `clean_inodes()` runs a recursive dedup/aggregation pass:

```rust
if !use_apparent_size && let Some(id) = x.inode_device && !inodes.insert(id) {
    return None;
}
...
let actual_size = x.size + new_children.iter().map(|c| c.size).sum::<u64>();
```

For filetime mode, aggregation becomes `max(child.size)` instead of sum.

### Top-N display efficiency

`filter::get_biggest()` uses `BinaryHeap<&Node>` as a priority frontier and selects up to `number_of_lines` allowed nodes:

```rust
while allowed_nodes.len() < display_data.number_of_lines {
    if let Some(line) = heap.pop() {
        allowed_nodes.insert(line.name.as_path(), line);
        heap = add_children(&display_data, line, heap);
    } else { break; }
}
```

This avoids rendering entire huge trees when only top lines are needed. Then it rebuilds a display tree from selected nodes.

### Cross-device policy

When `limit_filesystem` is enabled, it computes root device IDs and filters entries not on allowed devices.

### Memory characteristics

- Holds full recursive `Node` tree before filtering display, so peak memory scales with traversed entry count.
- Additional short-lived structures: dedup set, heap frontier, allowed-node map.
- Strong for interactive-ish pretty output, less ideal for strict constant-memory batch summarization.

---

## 4) volscan deep dive

`volscan` is architecturally close to `dirscan` (effectively a modernized fork with near-identical hot path).

### Parallelism model

Uses `jwalk` with explicit Rayon pool size:

```rust
WalkDir::new(path)
    .follow_links(false)
    .skip_hidden(self.ignore_hidden)
    .sort(true)
    .process_read_dir(... attach MetadataWithSize ...)
    .parallelism(Parallelism::RayonNewPool(self.threads))
```

CLI default thread count: `2 * num_cpus::get()`.

### Syscall and metadata strategy

In `process_read_dir`:
- fetches metadata once per entry (`dir_entry.metadata()`)
- file size:
  - `0` for directories
  - if `actual_size`: `path.size_on_disk_fast(&metadata)` fallback to `metadata.len()`
  - else: `metadata.len()`

So disk usage accounting depends on `filesize` crate behavior; apparent mode is direct metadata length.

### Aggregation and output structure

Key design: **streaming writer with minimal state**.

`WalkState` keeps only:
- `current: Option<DirectoryStat>`
- `writer`
- optional depth grouping

As entries arrive, it updates current stat or flushes and starts a new one.

`DirectoryStat` contains:
- `total_size`, `file_count`, `largest_file_size`, `path`
- `latest_created`, `latest_accessed`, `latest_modified`

So this tool captures richer metadata than most du-like scanners and writes JSON/CSV incrementally.

### Hard links, cross-device, memory mapping

- No inode dedup logic (hard links can be double-counted).
- No cross-device boundary filtering.
- No memory-mapped I/O usage.

### Performance characteristics

- Fast traversal from jwalk parallelism.
- Strong memory behavior from streaming output and tiny state.
- Additional metadata timestamp extraction increases per-entry cost vs size-only scanners.

Unique optimization: stream mode can emit plain file paths without JSON allocation-heavy formatting (`write!` path composition).

---

## 5) dirscan deep dive

`dirscan` hot path is functionally the same pattern as `volscan`:

- `jwalk` with `Parallelism::RayonNewPool(self.threads)`
- metadata attached in `process_read_dir`
- optional disk-usage sizing via `size_on_disk_fast`
- rolling `WalkState` + `DirectoryStat` flush
- JSON/CSV writer abstraction

### High-performance properties

The performance story is straightforward:

1. parallel walker (`jwalk`) does concurrent directory reads,
2. per-entry work is small,
3. output is streamed (bounded memory regardless of tree size),
4. no expensive in-memory global structure.

### Serialization format

- newline-delimited JSON objects (`JsonWriter` writes one serialized struct + newline)
- CSV rows with headers (`csv::WriterBuilder::has_headers(true)`)

### Memory-mapped I/O

None observed.

### Hard links / cross-device

No explicit dedup, no device-boundary filtering in scanner pipeline.

### Relationship to volscan

Code-level inspection shows extremely small deltas (primarily naming/packaging). For architecture decisions they can be treated as one family.

---

## 6) dutree deep dive

`dutree` is conceptually simpler and mostly serial.

### Traversal and tree construction

`Entry::new(path, cfg, depth)` recursively builds a full tree:

```rust
if path.is_dir() && (!cfg.depth_flag || depth > 0) {
    for entry in dir_list {
        let entry = Entry::new(&path, cfg, depth);
        if cfg.aggr > 0 && entry.bytes < cfg.aggr { aggr_bytes += entry.bytes; }
        else { vec.push(entry); }
    }
    vec.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes));
}
```

Directory bytes are then computed from children + own metadata bytes.

### Syscall strategy

Uses `symlink_metadata` and OS-specific `MetadataExt` APIs:
- Linux/FreeBSD: `st_blocks()*512` vs `st_size()`
- macOS: `blocks()*512` vs `size()`

Like `diskus`, this directly mirrors typical `du` disk-usage semantics.

### Parallelism, hard links, cross-device

- No parallelism (single-thread recursive traversal).
- No hard-link dedup policy.
- No cross-device filtering.

### Memory behavior

Builds and retains `Entry` tree (vector children per directory), then prints. Suitable for moderate trees; less suitable for very large volume-scale scans.

### Interesting Rust/tree patterns

- Recursive immutable-ish build returning `Entry` values.
- Post-build sorting by bytes.
- Aggregation bucket (`<aggregated>`) for entries below threshold.

Pattern is easy to reason about but not designed for extreme scale throughput.

---

## Cross-comparison matrix

| Aspect | diskus | dua-cli | dust | volscan | dirscan | dutree |
|---|---|---|---|---|---|---|
| Parallelism lib | `rayon` + `crossbeam-channel` | `jwalk` + Rayon + `crossbeam` | `rayon` (`par_bridge`) | `jwalk` + Rayon | `jwalk` + Rayon | none (serial) |
| Thread count strategy | default `3 * cores`, cap 64; overrideable | default macOS=3 else `cores` (via `0 -> num_cpus::get()`); overrideable | Rayon default or explicit `--threads`; custom stack-size policy | default `2 * cores`; overrideable | default `2 * cores`; overrideable | single thread |
| Walking strategy | custom recursive walk over `read_dir`, parallel per-level slices | `jwalk` iterator; aggregate mode streaming; interactive mode event-streamed tree integration | custom recursive `read_dir` + `par_bridge` + post-pass dedup | `jwalk` with `process_read_dir` metadata attachment | same as volscan | recursive `read_dir` tree build |
| Stat syscall path | `symlink_metadata` + Unix `blocks()*512` / `len()` | metadata via `jwalk`; size via `len()` or `size_on_disk_fast` | `symlink_metadata`/`metadata`; Unix blocks with NTFS guard; Windows fast/expensive branches | `dir_entry.metadata()`, `len()` or `size_on_disk_fast` | same as volscan | `symlink_metadata`; OS `MetadataExt` blocks/size |
| Hard link dedup | yes (`HashSet<(dev,ino)>` when `nlink>1`) | yes (`InodeFilter`) unless `count_hard_links` | yes (inode/device dedup pass), disabled in apparent-size mode | no | no | no |
| Cross-device control | none explicit | yes (`stay_on_filesystem` / device checks) | yes (`limit_filesystem` -> allowed device IDs) | no | no | no |
| Memory pattern | streaming reduction + small sets/vectors | aggregate: streaming low memory; interactive: full `StableGraph` (higher memory) | full `Node` tree + heap/map for display selection | streaming `WalkState` + writer (near-constant memory) | same as volscan | full recursive tree |
| Streaming vs collect | mostly streaming | both (mode-dependent) | mostly collect then filter/render | streaming | streaming | collect |

---

## Common patterns across tools

1. **Metadata caching at directory-read boundary**
   - `jwalk` users attach metadata into per-entry client state during `process_read_dir`.
   - This avoids repeated stat calls later in aggregation.

2. **Size policy split: apparent vs allocated**
   Every tool distinguishes at least two notions of size (`len` vs disk usage) either directly or via `filesize` helpers.

3. **Symlink caution**
   Most avoid blindly following links (`follow_links(false)` defaults or explicit symlink checks) to prevent loops and semantic surprises.

4. **Error-tolerant scanning**
   Scanners continue on errors, increment counters, and produce partial outputs.

5. **Work partition by directory structure**
   Whether custom recursion or jwalk internals, parallelism aligns with independent directory subtrees.

---

## Anti-patterns / performance hazards to avoid

1. **Unbounded full-tree retention by default**
   Great for UI, dangerous at scale. `dua-cli` interactive and `dust` are justifiable because UX needs it, but batch scanners should stream.

2. **Missing hard-link policy**
   No dedup can inflate totals on hard-link-heavy systems (backup trees, package stores).

3. **No cross-device guard for root scans**
   Unexpected mount traversal can explode runtime and violate user expectations.

4. **Heavy per-entry formatting/allocations in hot loop**
   Keep serialization/output decoupled and buffered; avoid frequent string formatting while scanning.

5. **Recursive delete/walk without symlink discipline**
   `dua-cli`’s defensive deletion path is a good reminder: never recurse through symlinks during destructive operations.

---

## Best-of-breed techniques to adopt

### From `dua-cli`

- **`jwalk` + `process_read_dir` metadata attachment** for efficient parallel walk.
- **Device boundary filtering** integrated into traversal.
- **InodeFilter-style hard-link accounting policy toggle**.
- **Event-stream architecture** if we build an interactive mode (scanner thread -> bounded channel -> UI).

### From `diskus`

- **Dedicated reducer thread** pattern for simple lock-free accumulation.
- **Conservative and explicit worker heuristic** (`k * cores` with cap).
- **Minimal per-entry processing in hot path**.

### From `dust`

- **Cross-platform metadata abstraction layer** (`platform::get_metadata`) with platform-specific performance workarounds.
- **Top-N frontier selection with `BinaryHeap`** to avoid rendering everything.
- **Configurable stack strategy** when recursion depth risks stack overflow.

### From `volscan/dirscan`

- **Streaming JSON/CSV emission** with bounded memory regardless of total file count.
- **Simple `WalkState` contract** to keep scanner and serialization loosely coupled.

### From `dutree`

- **Threshold aggregation node (`<aggregated>`)** to compress noisy tails in human output.
- Useful for non-interactive textual tree rendering.

---

## Recommended dependency set for our scanner (`Cargo.toml`)

For a high-performance, correctness-focused scanner with optional UI:

```toml
[dependencies]
anyhow = \"1\"
serde = { version = \"1\", features = [\"derive\"] }
serde_json = \"1\"
csv = \"1\"

jwalk = \"0.8\"
rayon = \"1\"
crossbeam-channel = \"0.5\"

filesize = \"0.2\"
num_cpus = \"1\"

# Optional, if we keep hard-link dedup map large and want faster hashing:
ahash = \"0.8\"

# Optional interactive mode:
petgraph = \"0.8\"
ratatui = { version = \"0.26\", optional = true, default-features = false, features = [\"crossterm\"] }
crossterm = { version = \"0.27\", optional = true }
```

And optional features split:

```toml
[features]
default = [\"batch\"]
batch = []
interactive = [\"ratatui\", \"crossterm\", \"petgraph\"]
```

Rationale:
- `jwalk` gives robust parallel traversal primitives.
- `crossbeam-channel` is ideal for bounded event/reduction pipelines.
- `filesize` provides portable disk-usage helpers.
- UI dependencies remain optional so batch binary remains lean.

---

## Key takeaways for our disk scanner

1. **Define operating modes explicitly**
   - `scan --stream` (default): constant-memory, machine-readable output.
   - `scan --summary`: scalar totals and small top-K summaries.
   - optional `interactive` mode: full tree materialization only when requested.

2. **Make counting policy first-class and explicit**
   Required toggles:
   - `--apparent-size`
   - `--count-hard-links` (default off for dedup correctness)
   - `--one-file-system`
   - `--follow-links` (default false)

3. **Hot-path architecture recommendation**
   - `jwalk` iterator configured with `process_read_dir` to prefetch metadata.
   - Per-entry lightweight struct sent to bounded channel.
   - Single reducer thread updates totals + per-directory aggregates.
   - Optional writer thread flushes JSONL/CSV.

4. **Data structure guidance**
   - Batch default: hash maps keyed by compact directory IDs/paths only as needed; avoid full child vectors unless output requires tree.
   - Use inode dedup map with eviction strategy only when `nlink > 1` seen.
   - Keep per-entry allocations minimal (borrowed/owned path strategy carefully chosen).

5. **Threading guidance**
   - Start with `threads = num_cpus::get()` default (safe baseline).
   - Offer multiplier mode (`--threads-multiplier`) for HDD/cold-cache workloads.
   - Cap maximum worker count (similar to `diskus`) to avoid pathological oversubscription.

6. **Syscall strategy guidance**
   - Prefer `symlink_metadata` in walker to avoid accidental link traversal.
   - Use `size_on_disk_fast` for allocated-size mode where available.
   - Keep platform abstraction boundary isolated (dust-style `platform` module) so Windows and Unix optimizations can diverge without contaminating hot loop logic.

7. **Output strategy**
   - JSONL and CSV should be streaming writers, not in-memory vectors.
   - Human table/tree output should be generated from bounded summaries or explicit in-memory mode.

8. **Testing priorities inferred from these tools**
   - hard-link correctness across filesystems
   - mount-boundary behavior
   - symlink cycles and deletion safety (if mutation is supported)
   - deep directory recursion/stack robustness
   - large-entry-count memory ceiling tests

---

## Final assessment

- **Best raw minimal scanner pattern:** `diskus` (simple, focused, effective).
- **Best composable architecture for both batch and interactive:** `dua-cli`.
- **Best platform-specific metadata engineering:** `dust`.
- **Best constant-memory reporting model:** `volscan` / `dirscan`.
- **Best simple tree readability pattern:** `dutree`.

For our implementation in `hokori-rs`, the strongest path is to combine:

- `dua-cli`’s `jwalk` integration style,
- `diskus`/`dua` hard-link and reducer discipline,
- `dust` cross-platform metadata abstraction rigor,
- `dirscan`/`volscan` streaming writer model.

That combination should give us predictable memory, high throughput, and correct semantics on real-world heterogeneous filesystems.