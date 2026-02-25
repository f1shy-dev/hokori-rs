@@ -0,0 +1,435 @@
+# macOS/XNU filesystem traversal syscalls and dumac: design notes for a fastest-possible disk scanner
+
+## Executive summary
+
+- `getattrlistbulk(2)` is the critical macOS syscall for high-speed disk usage traversal because it batches directory enumeration and metadata fetch into one kernel/user transition, eliminating the `getdents64 + lstat` pattern that dominates traditional scanners.
+- In XNU, `getattrlistbulk` first tries filesystem-native `VNOP_GETATTRLISTBULK`; if unsupported or the request is “expensive,” it falls back to VFS emulation (`readdirattr` + `getattrlist_internal`) [[1:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4353-4414]].
+- dumac’s current Rust implementation is correctly tuned around this API: 128 KiB bulk buffer, tight unsafe parsing of packed variable-length entries, inode dedup with sharded mutex+hashset, and Rayon work-stealing recursion [[2:/home/dumac/src/main.rs:203-353]].
+- The Tokio-era design paid avoidable scheduling overhead because the core syscall (`getattrlistbulk`) is blocking. The author’s profiling/benchmark notes show Rayon’s fixed pool + work-stealing reduced context switches and improved runtime by ~23–28% in newer measurements.
+- For a “world’s fastest” scanner on macOS, the winning strategy is: minimal attribute bitmap, large bulk buffers, no per-entry syscalls, strict unsafe parser bounds checks, contention-aware hardlink dedup, and explicit policy for symlinks/mount boundaries.
+
+---
+
+## 1) Syscall inventory and signatures from XNU
+
+From `syscalls.master` and headers:
+
+- `getattrlist(path, alist, attributeBuffer, bufferSize, options)` syscall 220 [[3:/home/xnu-kernel/bsd/kern/syscalls.master:328-328]]
+- `searchfs(path, searchblock, nummatches, scriptcode, options, state)` syscall 225 [[4:/home/xnu-kernel/bsd/kern/syscalls.master:333-333]]
+- `fgetattrlist(fd, ...)` syscall 228 [[5:/home/xnu-kernel/bsd/kern/syscalls.master:336-336]]
+- `getdirentries64(fd, buf, bufsize, position)` syscall 344 [[6:/home/xnu-kernel/bsd/kern/syscalls.master:525-525]]
+- `getattrlistbulk(dirfd, alist, attributeBuffer, bufferSize, options)` syscall 461 [[7:/home/xnu-kernel/bsd/kern/syscalls.master:715-715]]
+
+Public userland declaration for `getattrlistbulk` in `unistd.h`:
+
+```c
+int getattrlistbulk(int, void *, void *, size_t, uint64_t);
+```
+
+[[8:/home/xnu-kernel/bsd/sys/unistd.h:189-190]]
+
+---
+
+## 2) Detailed `getattrlistbulk` analysis (XNU + practical implications)
+
+### 2.1 Kernel path and control flow
+
+`getattrlistbulk()` in `vfs_attrlist.c` does the following:
+
+1. Validates fd, read access, and directory vnode type [[9:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4223-4269]].
+2. Requires `ATTR_BULK_REQUIRED` in `commonattr` (`ATTR_CMN_NAME | ATTR_CMN_RETURNED_ATTRS`) and no `volattr` [[10:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4290-4293]] [[11:/home/xnu-kernel/bsd/sys/attr.h:594-596]].
+3. Applies auth policy: pure name/type/id requests can pass with `LIST_DIRECTORY`; richer attrs require `SEARCH` too [[12:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4303-4314]].
+4. Tries native FS implementation via `VNOP_GETATTRLISTBULK` [[13:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4387-4388]].
+5. Falls back to `readdirattr` emulation on `ENOTSUP` [[14:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4404-4414]].
+
+The offset is tracked in per-fd state (`fv_offset` / `fg_offset`) and reset semantics are explicit [[15:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4326-4339]].
+
+### 2.2 Why it beats Linux-style `getdents64 + stat`
+
+Linux/BSD classic pattern for directory size scanners:
+
+- `getdents64`/`readdir` to list names
+- `stat/lstat` for each entry to get size/type/inode
+
+This is many syscalls and user/kernel round-trips. On macOS, `getattrlistbulk` can return many entries with requested attrs in one shot; conceptually “`readdir + stat` fused.” That’s exactly why dumac and earlier prototypes outperform tools not using this API.
+
+### 2.3 Attribute bitmap and packing format
+
+Critical definitions in `attr.h`:
+
+- `struct attrlist` with groups `commonattr/volattr/dirattr/fileattr/forkattr` [[16:/home/xnu-kernel/bsd/sys/attr.h:82-90]].
+- `attribute_set_t` (returned attrs bitmask) [[17:/home/xnu-kernel/bsd/sys/attr.h:94-100]].
+- `attrreference_t { attr_dataoffset, attr_length }` for variable data [[18:/home/xnu-kernel/bsd/sys/attr.h:105-108]].
+
+For bulk calls, required mask:
+
+```c
+#define ATTR_BULK_REQUIRED (ATTR_CMN_NAME | ATTR_CMN_RETURNED_ATTRS)
+```
+
+[[11:/home/xnu-kernel/bsd/sys/attr.h:594-596]]
+
+XNU internal attr tables map bitmap bits to vnode attrs and sizes (`getattrlist_common_tab`, `getattrlist_file_tab`, etc.) [[19:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:550-607]].
+
+### 2.4 Buffer size behavior and practical optimum
+
+Two constraints matter:
+
+- XNU max attr buffer logic (`ATTR_MAX_BUFFER`, long-path variant) in attr API internals [[20:/home/xnu-kernel/bsd/sys/attr.h:134-141]].
+- Your app-level throughput sweet spot.
+
+dumac and the author’s benchmark notes both use **128 KiB** as the practical optimum for `getattrlistbulk` loops (enough entries per syscall without pathological cache churn). dumac hardcodes:
+
+```rust
+let mut attrbuf = [0u8; 128 * 1024];
+```
+
+[[21:/home/dumac/src/main.rs:218-220]]
+
+### 2.5 Returned buffer layout and dumac parser model
+
+Each entry starts with `u32 entry_length`, then fixed region containing `attribute_set_t` and requested fixed attrs, plus `attrreference_t` pointers into variable region.
+
+dumac parser flow:
+
+- read `entry_length`
+- read `returned_attrs`
+- parse `ATTR_CMN_NAME` via `attrreference_t`
+- optionally parse `ATTR_CMN_ERROR`, `ATTR_CMN_OBJTYPE`, `ATTR_CMN_FILEID`, `ATTR_FILE_ALLOCSIZE`
+- advance by `entry_length`
+
+[[22:/home/dumac/src/main.rs:246-343]]
+
+This matches XNU packing logic (`attrlist_pack_fixed/variable/string`, alignment rules) [[23:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:145-238]].
+
+---
+
+## 3) `getdirentries64` analysis
+
+### 3.1 Kernel behavior
+
+`getdirentries64` calls `getdirentries_common(..., VNODE_READDIR_EXTENDED)` [[24:/home/xnu-kernel/bsd/vfs/vfs_syscalls.c:11027-11048]].
+
+`vnode_readdir64`:
+
+- uses native extended readdir if FS advertises `VFC_VFSREADDIR_EXTENDED` [[25:/home/xnu-kernel/bsd/vfs/vfs_syscalls.c:10790-10794]]
+- otherwise does conversion path from legacy dirent to direntry, with smaller kernel buffer and copy/transform overhead [[26:/home/xnu-kernel/bsd/vfs/vfs_syscalls.c:10803-10879]]
+
+Extended-mode tail flag behavior:
+
+- if user buffer >= 1024, last 4 bytes can return `GETDIRENTRIES64_EOF` [[27:/home/xnu-kernel/bsd/sys/dirent_private.h:81-89]]
+
+### 3.2 Comparison vs `getattrlistbulk`
+
+`getdirentries64` returns names/types/dirent-ish fields, but not full file allocation metadata you need for `du`-style accounting. You still need separate stat-like work to get sizes. For disk usage scanners, that means extra syscalls and lower throughput.
+
+### 3.3 When to use it
+
+Use `getdirentries64` when:
+
+- you only need names/types/seek offsets
+- you are implementing a general directory iterator
+- you’re on FS/code paths where `getattrlistbulk` attrs are overkill
+
+For total-size scans: prefer `getattrlistbulk` almost always.
+
+---
+
+## 4) `searchfs` analysis
+
+### 4.1 What it is in XNU
+
+`searchfs` is a filesystem-mediated search API that takes two search parameter blobs + search attrs and returns matches into caller buffer via `VNOP_SEARCHFS` [[28:/home/xnu-kernel/bsd/vfs/vfs_syscalls.c:11521-11765]].
+
+It includes heavy parameter validation (including `attrreference_t` bounds checks for `ATTR_CMN_NAME`) [[29:/home/xnu-kernel/bsd/vfs/vfs_syscalls.c:11628-11671]].
+
+### 4.2 Why it is not ideal for full-volume usage scans
+
+- It is predicate-based search, not an exhaustive “all entries + allocsize” optimized walker.
+- Depends strongly on FS support (`VNOP_SEARCHFS` may return `ENOTSUP`; default stubs exist) [[30:/home/xnu-kernel/bsd/vfs/vfs_support.c:809-820]].
+- Designed around match limits/timelimits/state continuation, not deterministic complete accounting pass.
+
+Conclusion: useful for indexed/criteria search workloads, not the primary primitive for fastest `du`-equivalent traversal.
+
+---
+
+## 5) `getattrlist` (single-object) analysis
+
+`getattrlist_internal` is the core path for single object attrs:
+
+- validates bitmaps/options
+- sets up requested vnode attrs via table-driven mapping (`getattrlist_setupvattr`)
+- authorizes
+- fetches via `vnode_getattr`
+- packs via `vfs_attr_pack_internal`
+
+[[31:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:3250-3470]]
+
+Use it when:
+
+- one object path/fd must be queried precisely
+- bulk directory pass is not applicable
+
+For scanner traversal, per-entry `getattrlist` is too expensive compared with `getattrlistbulk`.
+
+---
+
+## 6) dumac code walkthrough (current Rust)
+
+### 6.1 FFI and syscall invocation
+
+dumac uses `libc::getattrlistbulk` directly with `libc::attrlist`:
+
+```rust
+let mut attrlist = libc::attrlist {
+    bitmapcount: libc::ATTR_BIT_MAP_COUNT as u16,
+    reserved: 0,
+    commonattr: libc::ATTR_CMN_RETURNED_ATTRS
+        | libc::ATTR_CMN_NAME
+        | ATTR_CMN_ERROR
+        | libc::ATTR_CMN_OBJTYPE
+        | libc::ATTR_CMN_FILEID,
+    volattr: 0,
+    dirattr: 0,
+    fileattr: libc::ATTR_FILE_ALLOCSIZE,
+    forkattr: 0,
+};
+```
+
+[[32:/home/dumac/src/main.rs:203-216]]
+
+Then loops until `retcount == 0` [[33:/home/dumac/src/main.rs:222-244]].
+
+### 6.2 Buffer parser internals
+
+Unsafe parser is pointer-based and mostly branch-minimal:
+
+- unaligned reads (`read_unaligned`) for each field
+- name extraction via `attrreference_t.attr_dataoffset`
+- skip `.` and `..`
+- ignore error entries (`ATTR_CMN_ERROR`)
+- switch by `obj_type`
+
+[[22:/home/dumac/src/main.rs:246-343]]
+
+This mirrors XNU’s packed ABI and avoids extra allocation/copy per entry except strings pushed into `subdirs`.
+
+### 6.3 Hardlink dedup strategy
+
+Global sharded structure:
+
+```rust
+static SEEN_INODES: LazyLock<[Mutex<HashSet<u64>>; SHARD_COUNT]> = ...
+```
+
+with `SHARD_COUNT = 128` and shard index derived from inode bits [[34:/home/dumac/src/main.rs:20-42]].
+
+`check_and_add_inode` returns blocks only on first-seen inode [[35:/home/dumac/src/main.rs:53-61]].
+
+This is a good pattern: lower contention than one global lock, low implementation complexity.
+
+### 6.4 Threading model: Tokio -> Rayon evolution
+
+Current repo uses Rayon only (`rayon` dependency; no tokio in active source) [[36:/home/dumac/Cargo.toml:6-10]] [[37:/home/dumac/src/main.rs:2-2]].
+
+From the author’s optimization notes:
+
+- old design: Tokio task-per-directory + `spawn_blocking` around `getattrlistbulk`
+- issue: blocking syscall in async runtime caused extra thread scheduling/communication overhead
+- new design: Rayon fixed pool + work-stealing recursive traversal
+- observed: major reduction in scheduling-related syscalls/context switches and ~23–28% speedup on large benchmark
+
+The reasoning is correct for this workload: almost entirely CPU + blocking kernel calls, minimal true async I/O benefits.
+
+### 6.5 Symlink and mount boundary behavior
+
+- Symlink handling: `VLNK` counted as 1 block (`du`-like behavior) [[38:/home/dumac/src/main.rs:332-335]].
+- Mount boundary handling: **no explicit device-id boundary check** in current code. Traversal recurses by path join only; it can cross mounts if they appear as subdirectories.
+
+If parity with `du -x` is required, add `ATTR_CMN_DEVID` and prune on dev mismatch.
+
+### 6.6 Memory allocation strategy
+
+- Fixed stack buffer: 128 KiB attr buffer per directory call site
+- Dynamic vectors for files/subdirs
+- Per-subdir path materialization as `String`
+- Global LazyLock sharded HashSets for inode dedup
+
+Simple and fast, though path string churn can still be optimized (arena or small-string path segments).
+
+---
+
+## 7) XNU VFS-layer mechanics relevant to scanner speed
+
+### 7.1 Native bulk VNOP vs fallback emulation
+
+The main high-performance gate is whether FS provides efficient `VNOP_GETATTRLISTBULK`. If not, VFS fallback does readdir + per-entry lookup/getattr packing, which is inherently heavier [[14:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4404-4414]] [[39:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:3970-4093]].
+
+### 7.2 vnode lifecycle effects during traversal
+
+Two notable points:
+
+- For native bulk path, XNU sets `UT_KERN_RAGE_VNODES` around operation to age created vnodes aggressively [[40:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4382-4390]].
+- Fallback path repeatedly does `namei` / `vnode_put` per entry [[39:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:4060-4099]], increasing vnode churn.
+
+This directly impacts cache pressure and lock traffic on huge trees.
+
+### 7.3 Directory entry buffering and cache behavior
+
+`refill_fd_direntries` keeps per-fd directory-entry buffers, adapts buffer size, and reuses state across calls [[41:/home/xnu-kernel/bsd/vfs/vfs_attrlist.c:3623-3759]]. This supports efficient enumeration and fewer repeated FS calls.
+
+### 7.4 APFS vs HFS+ notes (what can and cannot be concluded)
+
+From in-tree code alone (with this sparse checkout), there is no direct APFS VNOP source to quantify implementation differences. However:
+
+- XNU VFS path is generic and relies on filesystem VNOP implementation quality.
+- Apple filesystem-dev quote (surfaced in dumac’s first write-up) explicitly notes HFS+ can often satisfy bulk attr reads from clustered directory metadata and avoid vnode creation in many cases.
+
+Practical implication: both APFS and HFS+ benefit from `getattrlistbulk`, but exact win magnitude is filesystem-implementation dependent. For final tuning, benchmark per filesystem.
+
+---
+
+## 8) Attribute bitmap reference for disk usage scanners
+
+Recommended minimal/extended sets:
+
+| Goal | commonattr | fileattr | dirattr | Notes |
+|---|---|---|---|---|
+| Minimal fast DU | `ATTR_CMN_RETURNED_ATTRS`, `ATTR_CMN_NAME`, `ATTR_CMN_OBJTYPE`, `ATTR_CMN_FILEID`, optional `ATTR_CMN_ERROR` | `ATTR_FILE_ALLOCSIZE` | `0` | dumac’s current fast path |
+| Better sparse/compression accounting | same as above | `ATTR_FILE_ALLOCSIZE`, `ATTR_FILE_DATAALLOCSIZE`, `ATTR_FILE_DATALENGTH` | `0` | helps compare logical vs allocated data |
+| Mount boundary control | add `ATTR_CMN_DEVID` | same | `0` | implement `-x` behavior |
+| Directory size accounting | include dir attrs when needed | `0` | `ATTR_DIR_ALLOCSIZE`, `ATTR_DIR_DATALENGTH` | FS-dependent usefulness |
+
+Bit definitions from `attr.h`:
+
+- `ATTR_CMN_NAME` `0x00000001`
+- `ATTR_CMN_OBJTYPE` `0x00000008`
+- `ATTR_CMN_FILEID` `0x02000000`
+- `ATTR_CMN_ERROR` `0x20000000`
+- `ATTR_CMN_RETURNED_ATTRS` `0x80000000`
+- `ATTR_FILE_ALLOCSIZE` `0x00000004`
+- `ATTR_FILE_DATALENGTH` `0x00000200`
+- `ATTR_FILE_DATAALLOCSIZE` `0x00000400`
+
+[[42:/home/xnu-kernel/bsd/sys/attr.h:413-460]] [[43:/home/xnu-kernel/bsd/sys/attr.h:543-553]]
+
+---
+
+## 9) Comparison summary: `getattrlistbulk` vs `getdirentries64` vs `searchfs`
+
+| Dimension | `getattrlistbulk` | `getdirentries64` | `searchfs` |
+|---|---|---|---|
+| Primary use | high-throughput entry+attrs enumeration | raw directory entry listing | criteria-based FS search |
+| Metadata richness | high (bitmap-selected attrs) | low-medium | configurable return attrs for matches |
+| Syscall count for DU | low | high (needs extra stat/getattr) | not intended for complete DU |
+| FS dependency | native VNOP ideal, VFS fallback exists | widely available | highly FS-support dependent |
+| Best for fastest scanner | **Yes** | No | No |
+
+---
+
+## 10) Recommended Rust implementation patterns for macOS
+
+### 10.1 FFI + parser skeleton (safe boundary + unsafe inner loop)
+
+```rust
+#[repr(C)]
+#[derive(Clone, Copy)]
+struct AttrList {
+    bitmapcount: u16,
+    reserved: u16,
+    commonattr: u32,
+    volattr: u32,
+    dirattr: u32,
+    fileattr: u32,
+    forkattr: u32,
+}
+
+#[repr(C)]
+#[derive(Clone, Copy)]
+struct AttrReference {
+    attr_dataoffset: i32,
+    attr_length: u32,
+}
+
+fn scan_dir_fd(dirfd: libc::c_int, out: &mut Vec<u8>) -> std::io::Result<()> {
+    let mut al = libc::attrlist {
+        bitmapcount: libc::ATTR_BIT_MAP_COUNT as u16,
+        reserved: 0,
+        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
+            | libc::ATTR_CMN_NAME
+            | libc::ATTR_CMN_OBJTYPE
+            | libc::ATTR_CMN_FILEID,
+        volattr: 0,
+        dirattr: 0,
+        fileattr: libc::ATTR_FILE_ALLOCSIZE,
+        forkattr: 0,
+    };
+
+    let mut buf = vec![0u8; 128 * 1024];
+    loop {
+        let n = unsafe {
+            libc::getattrlistbulk(
+                dirfd,
+                &mut al as *mut _ as *mut libc::c_void,
+                buf.as_mut_ptr() as *mut libc::c_void,
+                buf.len(),
+                0,
+            )
+        };
+        if n == 0 { break; }
+        if n < 0 { return Err(std::io::Error::last_os_error()); }
+
+        let mut p = 0usize;
+        for _ in 0..n {
+            if p + 4 > buf.len() { break; }
+            let ent_len = u32::from_ne_bytes(buf[p..p+4].try_into().unwrap()) as usize;
+            if ent_len == 0 || p + ent_len > buf.len() { break; }
+
+            let ent = &buf[p..p+ent_len];
+            // parse entry payload here with checked offsets before pointer reads
+            // ...
+            p += ent_len;
+        }
+    }
+    Ok(())
+}
+```
+
+Pattern recommendations:
+
+- Keep all unsafe reads localized to one parser module.
+- Validate `entry_length`, `attrreference_t` offsets, and NUL lengths before deref.
+- Separate scan policy from parser: dedup, symlink policy, mount policy, reporting.
+
+### 10.2 Concurrency model
+
+- Prefer Rayon or a custom fixed thread pool for this workload.
+- Cap threads by available fds and cores.
+- Avoid async runtime unless additional network/true async I/O is integral.
+
+### 10.3 Hardlink dedup pattern
+
+- Keep sharded set (`N=64..256`) with low-overhead mutexes (`parking_lot` is fine).
+- Use `fileid + devid` key if crossing filesystems is possible.
+
+---
+
+## 11) Key takeaways for building our scanner
+
+1. `getattrlistbulk` is the foundational syscall; anything else is a fallback path.
+2. Request only the attributes you actually need (every extra attr increases packing/parsing overhead).
+3. 128 KiB buffer is a strong baseline; tune 64/128/256 KiB experimentally on APFS targets.
+4. Implement strict parser bounds checks despite unsafe fast path.
+5. Use fixed-pool parallel recursion (Rayon-style work-stealing) over Tokio+blocking hybrids.
+6. Keep hardlink dedup contention low (sharded sets) and include `devid` in key for cross-mount correctness.
+7. Add explicit mode flags for symlink/mount behavior (`du` parity modes).
+8. Benchmark with warm and cold cache runs per filesystem; APFS/HFS+/network mounts can behave very differently.
+
+---
+
+## Appendix: benchmark context from dumac project notes
+
+- Initial “Maybe fastest” report: `dumac` ~6.39x faster than `du -sh`, ~2.58x faster than `diskus` on the cited deep benchmark; author rounds to 6.4x and 2.58x.
+- Follow-up optimization report: switching from Tokio approach to Rayon + contention tweaks yielded another ~23–28% speedup depending on run set.
+
+These results are consistent with syscall-bound traversal where reducing scheduler/context-switch overhead materially improves end-to-end runtime.