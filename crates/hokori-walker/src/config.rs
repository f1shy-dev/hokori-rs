use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WalkConfig {
    pub roots: Vec<PathBuf>,
    pub threads: usize,
    pub follow_symlinks: bool,
    pub same_filesystem: bool,
    pub max_depth: usize,
    // PERF: 4096 entries × ~120 bytes = ~480KB of channel buffer. This is a good balance:
    // too small and walker threads block waiting for the scanner to drain, too large and
    // we waste memory. In practice the channel rarely fills because the scanner loop is
    // faster than per-file stat calls. If profiling shows producers blocking, increase this
    // or consider multiple channels (one per worker group).
    pub channel_capacity: usize,
}

impl Default for WalkConfig {
    fn default() -> Self {
        Self {
            roots: vec![],
            threads: 0,
            follow_symlinks: false,
            same_filesystem: true,
            max_depth: 0,
            channel_capacity: 4096,
        }
    }
}

impl WalkConfig {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            ..Default::default()
        }
    }

    pub fn resolved_threads(&self) -> usize {
        // PERF: Default thread count capped at 32 to avoid diminishing returns from
        // context switching. For NVMe/SSD with fast metadata ops, more threads help
        // (up to ~64). For spinning disks, 2-4 threads is optimal to avoid seek storms.
        // Consider: expose a --disk-type flag or auto-detect via /sys/block/*/queue/rotational.
        if self.threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get().min(32))
                .unwrap_or(4)
        } else {
            self.threads
        }
    }
}
