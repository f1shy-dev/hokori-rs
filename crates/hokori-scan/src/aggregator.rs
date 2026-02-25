use crate::tree::TreeNode;

#[derive(Debug, Clone, Default)]
pub struct DirStats {
    pub direct_size: u64,
    pub total_size: u64,
    pub direct_count: u64,
    pub total_count: u64,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub total_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub error_count: u64,
    pub deduped_count: u64,
    pub skipped_count: u64,
    pub roots: Vec<RootResult>,
    pub tree: Option<Vec<TreeNode>>,
}

#[derive(Debug, Clone)]
pub struct RootResult {
    pub path: std::path::PathBuf,
    pub total_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
}

impl Default for ScanResult {
    fn default() -> Self {
        Self {
            total_size: 0,
            file_count: 0,
            dir_count: 0,
            error_count: 0,
            deduped_count: 0,
            skipped_count: 0,
            roots: vec![],
            tree: None,
        }
    }
}

pub struct StreamingAggregator {
    result: ScanResult,
}

impl StreamingAggregator {
    pub fn new() -> Self {
        Self {
            result: ScanResult::default(),
        }
    }

    pub fn add_entry(&mut self, size: u64, is_dir: bool) {
        self.result.total_size += size;
        if is_dir {
            self.result.dir_count += 1;
        } else {
            self.result.file_count += 1;
        }
    }

    pub fn add_deduped(&mut self) {
        self.result.deduped_count += 1;
    }

    pub fn add_error(&mut self) {
        self.result.error_count += 1;
    }

    pub fn add_skipped(&mut self) {
        self.result.skipped_count += 1;
    }

    pub fn add_root_result(&mut self, root: RootResult) {
        self.result.roots.push(root);
    }

    pub fn set_tree(&mut self, tree: Option<Vec<TreeNode>>) {
        self.result.tree = tree;
    }

    pub fn finish(self) -> ScanResult {
        self.result
    }
}

impl Default for StreamingAggregator {
    fn default() -> Self {
        Self::new()
    }
}
