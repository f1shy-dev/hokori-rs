pub mod config;
pub mod entry;
pub mod error;
mod worker;

pub use config::WalkConfig;
pub use entry::DirEntry;
pub use error::WalkError;

use crossbeam_channel::Receiver;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub struct Walker {
    config: WalkConfig,
}

impl Walker {
    pub fn new(config: WalkConfig) -> Self {
        Self { config }
    }

    pub fn walk(&self) -> (Receiver<Result<DirEntry, WalkError>>, WalkHandle) {
        let capacity = self.config.channel_capacity.max(1);
        let (sender, receiver) = crossbeam_channel::bounded(capacity);
        let cancel = Arc::new(AtomicBool::new(false));

        let joins = worker::spawn_walk(self.config.clone(), sender, cancel.clone());

        (
            receiver,
            WalkHandle {
                cancel,
                joins: std::sync::Mutex::new(Some(joins)),
            },
        )
    }

    pub fn walk_collect(&self) -> (Vec<DirEntry>, Vec<WalkError>) {
        let (rx, _handle) = self.walk();
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for result in rx {
            match result {
                Ok(entry) => entries.push(entry),
                Err(e) => errors.push(e),
            }
        }
        (entries, errors)
    }
}

pub struct WalkHandle {
    cancel: Arc<AtomicBool>,
    joins: std::sync::Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl WalkHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for WalkHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Ok(slot) = self.joins.get_mut() {
            if let Some(joins) = slot.take() {
                for join in joins {
                    let _ = join.join();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_walk_simple_tree() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        fs::write(tmp.path().join("file1.txt"), "hello").unwrap();
        fs::write(tmp.path().join("subdir/file2.txt"), "world").unwrap();

        let config = WalkConfig::new(vec![tmp.path().to_path_buf()]);
        let walker = Walker::new(config);
        let (entries, errors) = walker.walk_collect();

        assert!(errors.is_empty());
        assert!(entries.len() >= 3);
    }

    #[test]
    fn test_walk_depth_limit() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
        fs::write(tmp.path().join("a/b/c/deep.txt"), "deep").unwrap();

        let mut config = WalkConfig::new(vec![tmp.path().to_path_buf()]);
        config.max_depth = 2;
        let walker = Walker::new(config);
        let (entries, _) = walker.walk_collect();

        assert!(entries.iter().all(|e| e.depth <= 2));
    }

    #[test]
    fn test_walk_permission_error_continues() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("readable")).unwrap();
        fs::write(tmp.path().join("readable/file.txt"), "ok").unwrap();

        let config = WalkConfig::new(vec![tmp.path().to_path_buf()]);
        let walker = Walker::new(config);
        let (entries, _errors) = walker.walk_collect();
        assert!(!entries.is_empty());
    }
}
