use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WalkConfig {
    pub roots: Vec<PathBuf>,
    pub threads: usize,
    pub follow_symlinks: bool,
    pub same_filesystem: bool,
    pub max_depth: usize,
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
        if self.threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get().min(32))
                .unwrap_or(4)
        } else {
            self.threads
        }
    }
}
