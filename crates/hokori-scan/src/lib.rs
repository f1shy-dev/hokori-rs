use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Receiver;
use hokori_walker::error::WalkErrorKind;
use hokori_walker::{WalkConfig, WalkError, Walker};

use crate::tree::TreeBuilder;

pub mod aggregator;
pub mod dedup;
pub mod progress;
pub mod tree;

pub use aggregator::ScanResult;
pub use progress::ScanProgress;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeMode {
    DiskUsage,
    ApparentSize,
}

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub threads: usize,
    pub size_mode: SizeMode,
    pub dedup_hardlinks: bool,
    pub follow_symlinks: bool,
    pub same_filesystem: bool,
    pub max_depth: usize,
    pub build_tree: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: vec![],
            threads: 0,
            size_mode: SizeMode::DiskUsage,
            dedup_hardlinks: true,
            follow_symlinks: false,
            same_filesystem: true,
            max_depth: 0,
            build_tree: false,
        }
    }
}

impl ScanConfig {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            ..Default::default()
        }
    }
}

pub struct Scanner {
    config: ScanConfig,
}

impl Scanner {
    pub fn new(config: ScanConfig) -> Self {
        Self { config }
    }

    pub fn scan(&self) -> ScanHandle {
        let (progress_tx, progress_rx) = crossbeam_channel::bounded(16);
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));

        let config = self.config.clone();
        let cancel_clone = cancel.clone();

        std::thread::spawn(move || {
            let walk_config = WalkConfig {
                roots: config.roots.clone(),
                threads: config.threads,
                follow_symlinks: config.follow_symlinks,
                same_filesystem: config.same_filesystem,
                max_depth: config.max_depth,
                ..Default::default()
            };

            let mut root_results: Vec<aggregator::RootResult> = config
                .roots
                .iter()
                .cloned()
                .map(|path| aggregator::RootResult {
                    path,
                    total_size: 0,
                    file_count: 0,
                    dir_count: 0,
                })
                .collect();
            let root_path_bytes: Vec<Vec<u8>> = config
                .roots
                .iter()
                .map(|path| path.as_os_str().as_bytes().to_vec())
                .collect();

            let walker = Walker::new(walk_config);
            let (entry_rx, walk_handle) = walker.walk();

            let dedup_filter = if config.dedup_hardlinks {
                Some(dedup::InodeDedup::new())
            } else {
                None
            };

            let mut aggregator = aggregator::StreamingAggregator::new();
            let mut tree_builder = config.build_tree.then(TreeBuilder::new);
            let mut progress = progress::ProgressTracker::new(progress_tx);
            let mut errors: Vec<WalkError> = Vec::new();

            for result in entry_rx {
                if cancel_clone.load(Ordering::Relaxed) {
                    walk_handle.cancel();
                    break;
                }

                match result {
                    Ok(entry) => {
                        if progress.should_update() {
                            let path = OsStr::from_bytes(entry.path_bytes())
                                .to_string_lossy()
                                .into_owned();
                            progress.set_current_path(path);
                        }

                        let root_idx = root_path_bytes
                            .iter()
                            .position(|root| path_has_prefix(entry.path_bytes(), root));

                        let size = match config.size_mode {
                            SizeMode::DiskUsage => {
                                entry.disk_usage.or(entry.apparent_size).unwrap_or(0)
                            }
                            SizeMode::ApparentSize => {
                                entry.apparent_size.or(entry.disk_usage).unwrap_or(0)
                            }
                        };
                        let size_apparent = entry.apparent_size.unwrap_or(0);
                        let size_disk = entry.disk_usage.unwrap_or(0);

                        if entry.is_file() {
                            if let Some(ref dedup) = dedup_filter {
                                if !dedup.check_and_insert(entry.dev, entry.ino) {
                                    aggregator.add_deduped();
                                    continue;
                                }
                            }

                            aggregator.add_entry(size, false);
                            progress.record_file(size);

                            if let Some(ref mut builder) = tree_builder {
                                builder.insert(
                                    entry.path_bytes(),
                                    size_apparent,
                                    size_disk,
                                    false,
                                    entry.depth,
                                );
                            }

                            if let Some(idx) = root_idx {
                                root_results[idx].total_size += size;
                                root_results[idx].file_count += 1;
                            }
                        } else if entry.is_dir() {
                            aggregator.add_entry(0, true);
                            progress.record_dir();

                            if let Some(ref mut builder) = tree_builder {
                                builder.insert(
                                    entry.path_bytes(),
                                    size_apparent,
                                    size_disk,
                                    true,
                                    entry.depth,
                                );
                            }

                            if let Some(idx) = root_idx {
                                root_results[idx].dir_count += 1;
                            }
                        } else {
                            aggregator.add_skipped();
                        }
                    }
                    Err(err) => {
                        aggregator.add_error();
                        progress.record_error();
                        errors.push(err);
                    }
                }
            }

            progress.finish();
            for root in root_results {
                aggregator.add_root_result(root);
            }
            if let Some(builder) = tree_builder {
                aggregator.set_tree(Some(builder.build(&config.roots)));
            }
            let result = aggregator.finish();
            let _ = result_tx.send((result, errors));
        });

        ScanHandle {
            progress: progress_rx,
            result_rx,
            cancel,
        }
    }

    pub fn scan_blocking(&self) -> (ScanResult, Vec<WalkError>) {
        self.scan().wait()
    }
}

pub struct ScanHandle {
    pub progress: Receiver<ScanProgress>,
    result_rx: Receiver<(ScanResult, Vec<WalkError>)>,
    cancel: Arc<AtomicBool>,
}

fn path_has_prefix(path: &[u8], root: &[u8]) -> bool {
    if root.is_empty() {
        return true;
    }

    if root == b"/" {
        return path.starts_with(root);
    }

    if path == root {
        return true;
    }

    if !path.starts_with(root) {
        return false;
    }

    if root.last() == Some(&b'/') {
        true
    } else {
        path.get(root.len()) == Some(&b'/')
    }
}

impl ScanHandle {
    pub fn wait(self) -> (ScanResult, Vec<WalkError>) {
        match self.result_rx.recv() {
            Ok(result) => result,
            Err(_) => {
                let mut result = ScanResult::default();
                result.error_count = 1;
                (
                    result,
                    vec![WalkError {
                        path: None,
                        depth: 0,
                        kind: WalkErrorKind::Io(std::io::Error::other(
                            "scan thread terminated unexpectedly",
                        )),
                    }],
                )
            }
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_scan_simple() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        fs::write(tmp.path().join("b.txt"), "world!").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/c.txt"), "deep").unwrap();

        let config = ScanConfig::new(vec![tmp.path().to_path_buf()]);
        let scanner = Scanner::new(config);
        let (result, errors) = scanner.scan_blocking();

        assert_eq!(result.file_count, 3);
        assert_eq!(result.dir_count, 1);
        assert!(result.total_size > 0);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_dedup_sharded() {
        let dedup = dedup::InodeDedup::new();
        assert!(dedup.check_and_insert(1, 100));
        assert!(!dedup.check_and_insert(1, 100));
        assert!(dedup.check_and_insert(1, 200));
        assert!(dedup.check_and_insert(2, 100));
    }

    #[test]
    fn test_progress_throttle() {
        let (tx, rx) = crossbeam_channel::bounded(100);
        let mut tracker = progress::ProgressTracker::new(tx);
        for _ in 0..10000 {
            tracker.record_file(100);
        }
        tracker.finish();
        let count = rx.try_iter().count();
        assert!(count < 200, "Progress should be throttled, got {count}");
        assert!(count > 0, "Should have at least one progress update");
    }

    #[test]
    fn test_path_has_prefix_component_boundary() {
        assert!(path_has_prefix(b"/tmp/root/file", b"/tmp/root"));
        assert!(path_has_prefix(b"./a/b", b"."));
        assert!(!path_has_prefix(b"/tmp/root2/file", b"/tmp/root"));
        assert!(!path_has_prefix(b"./abc", b"./a"));
    }
}
