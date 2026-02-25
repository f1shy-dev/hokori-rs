# Linux filesystem traversal syscalls + io_uring for ultra-fast disk scanning

## Executive summary (5 key findings)

1. **`getdents64` is still the traversal bottleneck syscall** for recursive scanners: the kernel side is already bulk-oriented (`iterate_dir()` + `filldir64()`), and user-space wins mostly by reducing call count with large buffers and avoiding per-entry follow-up syscalls when possible (`d_type`, `d_ino`) (`fs/readdir.c:85-117`, `349-426`).
2. **`statx` is the right metadata syscall for high-performance scanners** because it supports request masks (`STATX_SIZE`, `STATX_BLOCKS`, `STATX_INO`, etc.), richer fields than classic `stat`, and sync-control flags like `AT_STATX_DONT_SYNC` to avoid expensive remote metadata refreshes (`fs/stat.c:744-815`, `include/uapi/linux/stat.h:99-223`, `fs/stat.c:243-246`).
3. **`openat2` is materially safer for traversal than `openat`**: the `open_how.resolve` policy bits (`RESOLVE_NO_SYMLINKS`, `RESOLVE_BENEATH`, `RESOLVE_IN_ROOT`) are validated in-kernel and translated into lookup constraints, which is crucial for sandboxed/safe tree walking (`include/uapi/linux/openat2.h:19-42`, `fs/open.c:1181-1304`, `1391-1416`).
4. **Mainline io_uring currently supports metadata ops (`OPENAT`, `OPENAT2`, `STATX`, `CLOSE`) but not `GETDENTS`**. The opcode enum does not contain `IORING_OP_GETDENTS`, and op registration has no getdents entry (`include/uapi/linux/io_uring.h:252-321`, `io_uring/opdef.c:223-243`, `313-318`). Historical patchsets existed (liburing 2021 series + kernel patch discussion in 2023), but not merged into this kernel snapshot.
5. **Best practical design today is hybrid**: directory enumeration via `getdents64` (threaded, large buffers) + selective `statx` (possibly via io_uring batching). On HDD, disk-order heuristics (fastwalk-style extent sorting) can be dramatic; on SSD/NVMe, syscall minimization and concurrency control dominate.

---

## 1) Kernel syscall analysis

### 1.1 `getdents64` (`fs/readdir.c`)

#### Exact kernel signature

```c
SYSCALL_DEFINE3(getdents64, unsigned int, fd,
		struct linux_dirent64 __user *, dirent, unsigned int, count)
```

Source: `fs/readdir.c:397-399`.

The output struct is:

```c
struct linux_dirent64 {
	u64		d_ino;
	s64		d_off;
	unsigned short	d_reclen;
	unsigned char	d_type;
	char		d_name[];
};
```

Source: `include/linux/dirent.h` (via `git show HEAD:include/linux/dirent.h`).

#### Kernel-level behavior

`getdents64` sets up a callback context and calls `iterate_dir()` (`fs/readdir.c:401-413`). `iterate_dir()`:

- checks read permission (`security_file_permission`, `fsnotify_file_perm`),
- takes shared inode lock (`down_read_killable`),
- runs filesystem-specific directory iterator (`file->f_op->iterate_shared`),
- updates `f_pos` and access notifications.

Source: `fs/readdir.c:85-117`.

Representative kernel snippet:

```c
int iterate_dir(struct file *file, struct dir_context *ctx)
{
	...
	res = down_read_killable(&inode->i_rwsem);
	...
	res = file->f_op->iterate_shared(file, ctx);
	file->f_pos = ctx->pos;
	...
}
```

Each entry is copied to userspace in `filldir64()`:

- validates names (`verify_dirent_name`),
- computes record length with alignment,
- writes `d_ino`, `d_reclen`, `d_type`, name,
- updates cursor and remaining user buffer.

Source: `fs/readdir.c:349-389`.

#### Dentry cache and lookup implications

`getdents64` itself does not path-walk each child. It iterates the opened directory’s iterator (`iterate_shared`), so it is generally cheaper than per-entry `stat()` path lookups. The dcache still matters indirectly because filesystem iterators and inode/dentry state are often hot after previous traversals, but directory entry production is not equivalent to full child lookup.

#### `d_type` semantics and caveats

`d_type` is delivered from filesystem iterator into `filldir64()` and masked (`d_type &= S_DT_MASK`) before user copy (`fs/readdir.c:358-363`). Availability is filesystem-dependent:

- Often available and reliable on ext4 and tmpfs.
- Frequently `DT_UNKNOWN` on some filesystems/configurations (historically common on XFS/btrfs/network/overlay situations).

fastwalk explicitly warns and falls back to `stat()` recursion for unknown types (`fastwalk.c:181-185`, `234-264`; README caveat on XFS/btrfs `README.md:49-51`).

#### Performance characteristics

- One syscall returns many entries (bulk), amortizing transition overhead.
- Cost per call scales with `count` buffer size and directory entropy.
- Typical practical sweet spot: **64 KiB to 1 MiB** buffer per worker.
  - 64 KiB: low memory footprint, still good amortization.
  - 256 KiB: strong default for modern scanners.
  - 1 MiB: can reduce syscall count further on very large directories but may reduce cache locality if overused.

Because each syscall is user→kernel→user, reducing `getdents64` call count is usually first-order.

#### Rust invocation patterns

Raw syscall pattern:

```rust
use libc::{c_long, syscall, SYS_getdents64};

#[repr(C)]
struct LinuxDirent64 {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    d_name: [u8; 0],
}

fn getdents64(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe {
        syscall(
            SYS_getdents64 as c_long,
            fd,
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
```

`nix` has broad file APIs but high-throughput scanners often keep `getdents64` as raw syscall for full control and zero extra abstraction overhead.

---

### 1.2 `statx` (`fs/stat.c`)

#### Exact kernel signature

```c
SYSCALL_DEFINE5(statx,
		int, dfd, const char __user *, filename, unsigned, flags,
		unsigned int, mask,
		struct statx __user *, buffer)
```

Source: `fs/stat.c:804-807`.

#### Kernel-level behavior

`statx` path:

- validates mask and sync flags (`do_statx`),
- performs lookup via `vfs_statx` / `filename_lookup`,
- fetches attributes using `vfs_getattr`,
- copies expanded `statx` structure to userspace.

Source: `fs/stat.c:744-766`, `341-363`, `296-315`.

Lookup semantics are controlled by `statx_lookup_flags()`:

- follow symlink unless `AT_SYMLINK_NOFOLLOW`,
- allow automount unless `AT_NO_AUTOMOUNT`.

Source: `fs/stat.c:284-294`.

#### Extra fields over classic `stat`

`struct statx` provides (among others):

- `stx_btime` (creation/birth time),
- `stx_mnt_id`,
- direct I/O alignment fields,
- subvolume id,
- atomic write capability fields,
- explicit result mask `stx_mask`.

Source: `include/uapi/linux/stat.h:99-193`.

Mask bits relevant to scanner metadata extraction:

- `STATX_INO` (`stx_ino`),
- `STATX_SIZE` (`stx_size`),
- `STATX_BLOCKS` (`stx_blocks`),
- `STATX_BASIC_STATS` as baseline.

Source: `include/uapi/linux/stat.h:203-215`.

#### `AT_STATX_DONT_SYNC` impact

Kernel docs in `vfs_getattr` note:

- `AT_STATX_FORCE_SYNC`: force remote sync.
- `AT_STATX_DONT_SYNC`: suppress sync refresh.

Source: `fs/stat.c:243-246`.

For scanners on remote/slow filesystems, `AT_STATX_DONT_SYNC` avoids turning metadata scans into network consistency operations.

#### Performance characteristics

- Per-entry syscall if used naively after `getdents64`.
- Strongly tunable via mask: request only needed fields (e.g., size/blocks/ino).
- Can be batched under io_uring (`IORING_OP_STATX`) to reduce submit-side syscall overhead.

#### Rust invocation patterns

Raw syscall:

```rust
use libc::{c_long, syscall, SYS_statx, AT_FDCWD};

fn statx_size(path: &std::ffi::CStr) -> std::io::Result<libc::statx> {
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    let flags = libc::AT_NO_AUTOMOUNT | libc::AT_STATX_DONT_SYNC;
    let mask = libc::STATX_SIZE | libc::STATX_BLOCKS | libc::STATX_INO;
    let rc = unsafe {
        syscall(
            SYS_statx as c_long,
            AT_FDCWD,
            path.as_ptr(),
            flags,
            mask,
            &mut stx as *mut libc::statx,
        )
    };
    if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(stx) }
}
```

With `nix`, many projects still use `libc` for `statx` because it exposes newest flags quickly.

---

### 1.3 `openat2` (`fs/open.c`, `include/uapi/linux/openat2.h`)

#### Exact kernel signature

```c
SYSCALL_DEFINE4(openat2, int, dfd, const char __user *, filename,
		struct open_how __user *, how, size_t, usize)
```

Source: `fs/open.c:1391-1393`.

#### Kernel-level behavior

- userspace `open_how` is copied with size/version checks (`copy_struct_from_user`, bounds on `usize`).
- parsed by `build_open_flags()` with strict validation.
- resolve policy bits translated into `LOOKUP_*` flags.

Sources: `fs/open.c:1397-1416`, `1181-1304`.

Important policies:

- `RESOLVE_NO_SYMLINKS` → `LOOKUP_NO_SYMLINKS`,
- `RESOLVE_BENEATH` → `LOOKUP_BENEATH`,
- `RESOLVE_IN_ROOT` → `LOOKUP_IN_ROOT`,
- unknown bits rejected.

Sources: `include/uapi/linux/openat2.h:25-42`, `fs/open.c:1203-1208`, `1285-1295`.

#### Why this matters for safe traversal

For scanners that must not escape a root (security tools, sandboxes), `openat2` provides kernel-enforced traversal constraints impossible to implement race-free with userspace string checks alone.

#### Performance characteristics

`openat2` is not faster than `openat` by itself; it is **safer and more deterministic**. It can avoid expensive/redundant failed path resolution patterns in hardened walkers by encoding policy once.

#### Rust invocation pattern

```rust
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn openat2_dirfd(dfd: i32, path: &std::ffi::CStr) -> std::io::Result<i32> {
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY) as u64,
        mode: 0,
        resolve: (libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS) as u64,
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dfd,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(rc as i32) }
}
```

---

### 1.4 `newfstatat` / `fstatat` (`fs/stat.c`)

#### Exact kernel signature

```c
SYSCALL_DEFINE4(newfstatat, int, dfd, const char __user *, filename,
		struct stat __user *, statbuf, int, flag)
```

Source: `fs/stat.c:532-533`.

#### Kernel-level behavior

`newfstatat` calls `vfs_fstatat`, which routes to `vfs_statx(..., STATX_BASIC_STATS)` and forces `AT_NO_AUTOMOUNT` in that path (`vfs_fstatat` ORs it in).

Source: `fs/stat.c:365-375`, `532-542`.

`AT_SYMLINK_NOFOLLOW` prevents target dereference at lookup level (`statx_lookup_flags`).

Source: `fs/stat.c:284-294`.

#### Why still relevant

`fstatat` is universally available and useful fallback when `statx` is unavailable in older environments. For newest scanners, `statx` usually supersedes it.

#### Rust invocation

- `nix::sys::stat::fstatat` is ergonomic and portable.
- Raw syscall possible but less common since libc wrappers are mature.

---

### 1.5 `close_range` (`fs/file.c`)

#### Exact kernel signature

```c
SYSCALL_DEFINE3(close_range, unsigned int, fd, unsigned int, max_fd,
		unsigned int, flags)
```

Source: `fs/file.c:819-820`.

#### Kernel-level behavior

- validates flags (`CLOSE_RANGE_UNSHARE`, `CLOSE_RANGE_CLOEXEC` only),
- optionally unshares fd table if needed,
- marks range cloexec or closes range in bulk.

Source: `fs/file.c:825-856`.

#### Performance relevance for scanners

Not traversal-critical; useful for fast cleanup in worker processes, crash-safe teardown, or daemonized scanners that inherit noisy fd tables.

#### Rust invocation

```rust
fn close_all_from(fd: u32) -> std::io::Result<()> {
    let rc = unsafe { libc::syscall(libc::SYS_close_range, fd, u32::MAX, 0u32) };
    if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}
```

---

## 2) io_uring analysis for filesystem traversal

### 2.1 What filesystem ops are in mainline

From opcode enum and op definitions:

- `IORING_OP_OPENAT`
- `IORING_OP_OPENAT2`
- `IORING_OP_CLOSE`
- `IORING_OP_STATX`
- plus other path ops (`RENAMEAT`, `UNLINKAT`, `MKDIRAT`, `LINKAT`, etc.)

Sources: `include/uapi/linux/io_uring.h:271-281`, `288-293`; `io_uring/opdef.c:223-243`, `313-318`.

`IORING_OP_GETDENTS` is absent from enum and op table in this tree.

### 2.2 Is `IORING_OP_GETDENTS` merged?

**Not in this kernel snapshot** (no opcode enum entry, no opdef registration).

Historical status:

- 2021 liburing patch series added userspace prep helper proposal for getdents (`[PATCH v1 0/4] ... add getdents64 support`).
- 2023 kernel patch discussions for io_uring getdents existed (patchwork references), but not present in current mainline code analyzed here.

Given absence in `include/uapi/linux/io_uring.h` and `io_uring/opdef.c`, production scanners must assume no native io_uring getdents today.

### 2.3 SQE/CQE model and syscall amortization

Core model:

- app mmaps SQ/CQ rings (`IORING_OFF_SQ_RING`, `IORING_OFF_CQ_RING`, `IORING_OFF_SQES`),
- app posts many SQEs,
- one `io_uring_enter()` can submit many (`to_submit`) and optionally wait CQEs.

Sources: `include/uapi/linux/io_uring.h:543-617`, `592-600`; `io_uring/io_uring.c:2538-2644`.

Kernel batching detail (`io_submit_sqes`):

- consumes multiple SQEs in a loop,
- delays/aggregates head commits,
- amortizes ring bookkeeping.

Source: `io_uring/io_uring.c:2008-2061` (including explicit batching note at `1993-1999`).

Representative opcode registration snippet:

```c
[IORING_OP_OPENAT] = { .prep = io_openat_prep, .issue = io_openat },
[IORING_OP_CLOSE] = { .prep = io_close_prep, .issue = io_close },
[IORING_OP_STATX] = { .prep = io_statx_prep, .issue = io_statx },
[IORING_OP_OPENAT2] = { .prep = io_openat2_prep, .issue = io_openat2 },
```

(From `io_uring/opdef.c:223-243`, `313-318`; notably no `IORING_OP_GETDENTS` entry.)

Meaning for traversal:

- Great for metadata fanout (`statx`, open/close bursts).
- Limited for full recursive walk because enumeration (`getdents64`) still uses conventional syscall path.

### 2.4 `lsr` practical evidence (io_uring-driven `ls`)

`lsr` creates a ring with queue size 256 and submits opens/stats asynchronously (`src/main.zig:34`, `300-327`, `1108-1113`, `1250-1279`).

Representative `lsr` snippet:

```zig
const queue_size = 256;
var ring: ourio.Ring = try .init(allocator, queue_size);
_ = try ring.open(directory, .{ .DIRECTORY = true, .CLOEXEC = true }, 0, .{ ... });
...
_ = try io.stat(path, &entry.statx, .{ .cb = onCompletion, ... });
```

Notably, it comments that readlink isn’t via io_uring in its implementation (`src/main.zig:1103`, `1270`).

Its own benchmark table shows syscall count reduction versus traditional `ls` families (README syscall benchmark section `README.md:71-82`), consistent with batching benefits.

---

## 3) fastwalk disk-order strategy analysis

fastwalk’s design is explicitly HDD-oriented (`README.md:5-12`, `49-51`):

1. **Pass 1**: walk directories, collect entries/types/inodes (`fastwalk.c:472-480`, `140-191`).
2. **Inode sort**: `qsort` by inode for “fast stat” locality (`193-217`, `482-483`).
3. **Pass 2**: query extents via `FIEMAP` (`FS_IOC_FIEMAP`) and store physical offsets (`302-347`, `489-505`).
4. **Pass 3 (optional)**: sort extents by physical disk address and issue `readahead()` in that order (`229-232`, `510-527`).

Representative fastwalk snippet:

```c
sort_inodes();
...
get_disk(entries[i].name, fd, &entries[i]);
...
sort_extents();
for (i = 0; i < numextents; i++)
	readahead(fd->fd, ex->offset, ex->len);
```

If filesystem gives `DT_UNKNOWN`, it falls back to `stat()` to determine directories and continue recursion (`234-264`).

### Why inode sorting helps

On classic filesystems (especially ext* layouts), inode tables often have spatial locality related to allocation groups. Sorting inode numbers can improve metadata fetch patterns during extent/stat pass.

### HDD vs SSD/NVMe

- **HDD**: physical ordering and readahead reduce seeks dramatically.
- **SSD/NVMe**: seek minimization is less important; throughput depends more on queue depth, parallelism, and syscall overhead. Disk-order heuristics may still help cache behavior slightly but have smaller impact.

---

## 4) Recommended optimal Linux traversal strategy (for fastest scanner)

### 4.1 Minimum syscall sequence to traverse and get sizes

For each directory:

1. `openat2(parent_fd, name, O_RDONLY|O_DIRECTORY|O_CLOEXEC, resolve_policy)`
2. loop `getdents64(dir_fd, buf, buf_len)` until 0
3. for files requiring size/blocks/ino: `statx(dir_fd, child_name, flags, mask, ...)`
4. `close(dir_fd)` (or bulk `close_range` at shutdown boundaries)

If safe root confinement required, use `RESOLVE_BENEATH`/`RESOLVE_IN_ROOT` on every directory open.

### 4.2 Avoid redundant `stat()` calls

- If `d_type` is definitive and your scanner only needs type + name count, skip metadata syscall.
- If only size needed for regular files, call `statx` only when `d_type == DT_REG`.
- If `d_type == DT_UNKNOWN`, fallback to `statx`/`fstatat`.
- Cache `d_ino` from dirent where possible to assist dedup/hardlink policy.

### 4.3 `getdents64` buffer sizing

Recommended starting point:

- **256 KiB per worker** default.
- downshift to 64 KiB if memory constrained.
- upshift to 1 MiB for huge fanout directories after benchmarking.

Reasoning: large enough to amortize syscall transitions, small enough to stay cache-friendly and avoid excessive per-thread memory.

### 4.4 Thread model

Because io_uring lacks `GETDENTS` in mainline, use hybrid parallelism:

- **Traversal workers**: 1–N threads issuing `getdents64` + enqueueing child tasks.
- **Metadata workers**: either direct `statx` syscalls or io_uring batched `STATX` submits.

Practical defaults:

- HDD: 1–2 traversal threads, limited metadata parallelism (avoid seek thrash).
- SATA SSD: 2–4 traversal threads.
- NVMe: 4–8 traversal threads (or per-NUMA shard), plus bounded metadata queue.

Always cap outstanding directory FDs and outstanding statx requests.

### 4.5 io_uring vs thread pool

- **io_uring wins** when you can batch many independent metadata ops (open/stat/close), reducing submit syscall frequency and improving completion handling.
- **thread pool wins/simplifies** for mixed traversal workloads because `getdents64` remains synchronous syscall path anyway.
- **Best now**: threadpool for traversal + optional io_uring lane for metadata bursts.

### 4.6 Filesystem-specific notes

- **ext4**: usually good `d_type`; benefits from avoiding extra stats, good general baseline.
- **xfs**: may return `DT_UNKNOWN` in some cases/configs; scanner must fallback robustly.
- **btrfs**: similar caveats around `d_type`/metadata behavior; richer statx fields can still help.
- **tmpfs**: metadata often very fast; syscall overhead dominates, so batching and low-overhead parsing matter most.

---

## 5) Rust implementation patterns (production-oriented)

### 5.1 Robust dirent parsing loop

```rust
fn scan_dir_fd(fd: i32, mut on_entry: impl FnMut(&[u8], u8, u64)) -> std::io::Result<()> {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = getdents64(fd, &mut buf)?;
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        while off < n {
            let base = &buf[off..n];
            let d_reclen = u16::from_ne_bytes([base[16], base[17]]) as usize;
            if d_reclen == 0 || off + d_reclen > n {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad dirent"));
            }
            let d_ino = u64::from_ne_bytes(base[0..8].try_into().unwrap());
            let d_type = base[18];
            let name = &base[19..d_reclen];
            let nul = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            let name = &name[..nul];
            if name != b"." && name != b".." {
                on_entry(name, d_type, d_ino);
            }
            off += d_reclen;
        }
    }
    Ok(())
}
```

### 5.2 Selective `statx` masks

Use minimal masks for scanner objective (file size inventory):

- `STATX_TYPE | STATX_SIZE | STATX_BLOCKS | STATX_INO`
- add timestamps only if needed
- combine with `AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC`

### 5.3 Safe traversal with `openat2`

Open children relative to parent fd and enforce resolve policy. Avoid string-based absolute joins during traversal logic where possible; keep `dirfd` stack/task graph.

### 5.4 Cleanup

For worker-process model, `close_range(3, UINT_MAX, 0)` is efficient fail-safe cleanup before `exec`/handoff boundaries.

---

## 6) Performance comparison (estimated overhead model)

These are practical order-of-magnitude estimates for hot-cache/local FS on modern x86_64 Linux, intended for planning (not universal constants):

| Operation model | Syscalls per 1M entries (typical) | Relative overhead | Notes |
|---|---:|---|---|
| `getdents64` only (large dirs, 256KiB buf) | ~5k–30k | Lowest | Depends on filename density/record size |
| `getdents64` + `statx` every entry | ~1,005,000+ | Very high | Metadata syscall dominates |
| `getdents64` + `statx` only `DT_REG` | workload-dependent | Medium | Big win when many dirs/symlinks |
| `getdents64` + io_uring batched `STATX` | same logical ops, fewer submit syscalls | Medium-low | Better at high fanout metadata bursts |
| io_uring-only traversal | N/A today | N/A | No mainline `IORING_OP_GETDENTS` |

Context-switch/transition framing:

- Plain syscall path: one user→kernel→user transition per syscall.
- io_uring batched submit: one `io_uring_enter()` can submit many SQEs, reducing transition frequency (`io_uring/io_uring.c:2538-2604`, `2008-2061`).

---

## 7) Key takeaways for hokori-rs disk scanner

1. **Design around `getdents64` first** (buffer size + parser + worker sharding); that is the fundamental traversal throughput lever.
2. **Use `statx` with narrow masks** for required metadata only; avoid legacy `stat` unless compatibility fallback is needed.
3. **Trust `d_type` when present, handle `DT_UNKNOWN` without assumptions**.
4. **Use `openat2` resolve policies for safe-by-default traversal** in hostile/untrusted trees.
5. **Adopt hybrid concurrency now**: threaded enumeration, optional io_uring for metadata batching.
6. **Enable fastwalk-like extent/disk-order mode only for HDD-oriented deployments**; keep it optional and benchmark-gated.
7. **Benchmark per filesystem and medium** (ext4/xfs/btrfs/tmpfs; HDD/SATA SSD/NVMe), because bottlenecks shift from seeks to syscall and cache effects.

---

## Appendix: syscall signature index (quick reference)

- `getdents64`: `fs/readdir.c:397-399`
- `statx`: `fs/stat.c:804-807`
- `newfstatat`: `fs/stat.c:532-533`
- `openat2`: `fs/open.c:1391-1393`
- `close_range`: `fs/file.c:819-820`

And key io_uring references:

- opcode enum: `include/uapi/linux/io_uring.h:252-321`
- fs-related op registration: `io_uring/opdef.c:223-243`, `313-318`
- submission batching: `io_uring/io_uring.c:2008-2061`
- syscall entry: `io_uring/io_uring.c:2538-2644`