use crate::config::WalkConfig;
use crate::entry::DirEntry;
use crate::error::{WalkError, WalkErrorKind};
use crossbeam_channel::Sender;
use crossbeam_deque::{Injector, Steal, Stealer, Worker as CbWorker};
use hokori_sys::{FileType, RawDirEntry, SysError};
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

#[derive(Debug)]
struct WorkItem {
    path: Vec<u8>,
    depth: u16,
    root_dev: u64,
}

struct WalkerWorker {
    local: CbWorker<WorkItem>,
    worker_idx: usize,
    stealers: Arc<[Stealer<WorkItem>]>,
    injector: Arc<Injector<WorkItem>>,
    sender: Sender<Result<DirEntry, WalkError>>,
    cancel: Arc<AtomicBool>,
    pending_dirs: Arc<AtomicUsize>,
    follow_symlinks: bool,
    same_filesystem: bool,
    max_depth: usize,
    buf: Vec<u8>,
}

impl WalkerWorker {
    fn run(&mut self) {
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }

            match self.find_work() {
                Some(item) => {
                    self.process_directory(item);
                    self.pending_dirs.fetch_sub(1, Ordering::AcqRel);
                }
                None => {
                    if self.pending_dirs.load(Ordering::Acquire) == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            }
        }
    }

    fn find_work(&self) -> Option<WorkItem> {
        if let Some(item) = self.local.pop() {
            return Some(item);
        }

        loop {
            match self.injector.steal() {
                Steal::Success(item) => return Some(item),
                Steal::Empty => break,
                Steal::Retry => continue,
            }
        }

        for (idx, stealer) in self.stealers.iter().enumerate() {
            if idx == self.worker_idx {
                continue;
            }
            loop {
                match stealer.steal() {
                    Steal::Success(item) => return Some(item),
                    Steal::Empty => break,
                    Steal::Retry => continue,
                }
            }
        }

        None
    }

    fn process_directory(&mut self, item: WorkItem) {
        let path_cstr = match CString::new(item.path.clone()) {
            Ok(c) => c,
            Err(_) => {
                self.send_error(
                    &item.path,
                    item.depth,
                    WalkErrorKind::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path contains NUL byte",
                    )),
                );
                return;
            }
        };

        let fd = match hokori_sys::platform::open_dir(None, &path_cstr) {
            Ok(fd) => fd,
            Err(e) => {
                self.send_error(&item.path, item.depth, map_sys_error(e));
                return;
            }
        };

        let dot = CStr::from_bytes_with_nul(b".\0").expect("valid dot");
        let current_dev = match hokori_sys::platform::stat_entry(fd, dot) {
            Ok(meta) => meta.dev,
            Err(e) => {
                hokori_sys::platform::close_fd(fd);
                self.send_error(&item.path, item.depth, map_sys_error(e));
                return;
            }
        };

        if self.same_filesystem && current_dev != item.root_dev {
            hokori_sys::platform::close_fd(fd);
            return;
        }

        let mut child_dirs = Vec::new();
        let next_depth = item.depth.saturating_add(1);
        let allow_descend = self.max_depth == 0 || (next_depth as usize) < self.max_depth;

        let sender = self.sender.clone();
        let cancel = self.cancel.clone();
        let follow_symlinks = self.follow_symlinks;

        let mut entry_path_buf = Vec::with_capacity(item.path.len().saturating_add(256));
        entry_path_buf.extend_from_slice(&item.path);
        if !entry_path_buf.is_empty() && entry_path_buf.last() != Some(&b'/') {
            entry_path_buf.push(b'/');
        }
        let entry_prefix_len = entry_path_buf.len();
        let mut name_c_buf = Vec::new();

        let result =
            hokori_sys::platform::read_dir_raw(fd, &mut self.buf, &mut |raw: RawDirEntry| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }

                entry_path_buf.truncate(entry_prefix_len);
                entry_path_buf.extend_from_slice(&raw.name);

                match raw.file_type {
                    FileType::Directory => {
                        let entry = DirEntry::from_parts(
                            entry_path_buf.clone(),
                            next_depth,
                            FileType::Directory,
                            raw.ino,
                            raw.size,
                            raw.size,
                            current_dev,
                            None,
                        );
                        if sender.send(Ok(entry)).is_err() {
                            cancel.store(true, Ordering::Relaxed);
                            return;
                        }
                        if allow_descend {
                            child_dirs.push(entry_path_buf.clone());
                        }
                    }
                    FileType::Unknown => {
                        let entry_path = entry_path_buf.clone();
                        if let Some(name_c) = name_to_cstr(&raw.name, &mut name_c_buf) {
                            match hokori_sys::platform::stat_entry(fd, name_c) {
                                Ok(meta) => {
                                    let entry = DirEntry::from_parts(
                                        entry_path,
                                        next_depth,
                                        meta.file_type,
                                        meta.ino,
                                        Some(meta.size),
                                        Some(meta.alloc_size),
                                        meta.dev,
                                        Some(meta.nlink),
                                    );
                                    if sender.send(Ok(entry)).is_err() {
                                        cancel.store(true, Ordering::Relaxed);
                                        return;
                                    }
                                    if allow_descend && meta.file_type == FileType::Directory {
                                        child_dirs.push(entry_path_buf.clone());
                                    }
                                }
                                Err(_) => {
                                    let entry = DirEntry::from_parts(
                                        entry_path,
                                        next_depth,
                                        FileType::Unknown,
                                        raw.ino,
                                        None,
                                        None,
                                        current_dev,
                                        None,
                                    );
                                    if sender.send(Ok(entry)).is_err() {
                                        cancel.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                        } else {
                            let entry = DirEntry::from_parts(
                                entry_path,
                                next_depth,
                                FileType::Unknown,
                                raw.ino,
                                None,
                                None,
                                current_dev,
                                None,
                            );
                            if sender.send(Ok(entry)).is_err() {
                                cancel.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    FileType::Symlink => {
                        let entry_path = entry_path_buf.clone();
                        let entry = DirEntry::from_parts(
                            entry_path.clone(),
                            next_depth,
                            FileType::Symlink,
                            raw.ino,
                            raw.size,
                            raw.size,
                            current_dev,
                            None,
                        );
                        if sender.send(Ok(entry)).is_err() {
                            cancel.store(true, Ordering::Relaxed);
                            return;
                        }
                        if follow_symlinks && allow_descend {
                            let path = PathBuf::from(OsStr::from_bytes(&entry_path));
                            if std::fs::metadata(&path)
                                .map(|m| m.is_dir())
                                .unwrap_or(false)
                            {
                                child_dirs.push(entry_path);
                            }
                        }
                    }
                    _ => {
                        let (apparent, disk, ino, dev, nlink) =
                            if let Some(name_c) = name_to_cstr(&raw.name, &mut name_c_buf) {
                                match hokori_sys::platform::stat_entry(fd, name_c) {
                                    Ok(meta) => (
                                        Some(meta.size),
                                        Some(meta.alloc_size),
                                        meta.ino,
                                        meta.dev,
                                        Some(meta.nlink),
                                    ),
                                    Err(_) => (raw.size, raw.size, raw.ino, current_dev, None),
                                }
                            } else {
                                (raw.size, raw.size, raw.ino, current_dev, None)
                            };

                        let entry = DirEntry::from_parts(
                            entry_path_buf.clone(),
                            next_depth,
                            raw.file_type,
                            ino,
                            apparent,
                            disk,
                            dev,
                            nlink,
                        );
                        if sender.send(Ok(entry)).is_err() {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                }
            });

        if let Err(e) = result {
            self.send_error(&item.path, item.depth, map_sys_error(e));
        }

        hokori_sys::platform::close_fd(fd);

        if !self.cancel.load(Ordering::Relaxed) {
            for child_path in child_dirs {
                self.pending_dirs.fetch_add(1, Ordering::AcqRel);
                self.local.push(WorkItem {
                    path: child_path,
                    depth: next_depth,
                    root_dev: item.root_dev,
                });
            }
        }
    }

    fn send_error(&self, path_bytes: &[u8], depth: u16, kind: WalkErrorKind) {
        let path = PathBuf::from(OsStr::from_bytes(path_bytes));
        let _ = self.sender.send(Err(WalkError {
            path: Some(path),
            depth,
            kind,
        }));
    }
}

fn name_to_cstr<'a>(name: &[u8], buf: &'a mut Vec<u8>) -> Option<&'a CStr> {
    if name.contains(&0) {
        return None;
    }

    buf.clear();
    buf.extend_from_slice(name);
    buf.push(0);
    CStr::from_bytes_with_nul(buf.as_slice()).ok()
}

fn map_sys_error(e: SysError) -> WalkErrorKind {
    const EACCES: i32 = 13;
    const EPERM: i32 = 1;
    const ENFILE: i32 = 23;
    const EMFILE: i32 = 24;

    match e {
        SysError::Io(io) => match io.raw_os_error() {
            Some(code) if code == EACCES || code == EPERM => WalkErrorKind::PermissionDenied,
            Some(code) if code == EMFILE || code == ENFILE => WalkErrorKind::TooManyOpenFiles,
            _ => WalkErrorKind::Io(io),
        },
        _ => WalkErrorKind::Io(std::io::Error::other("syscall failed")),
    }
}

fn path_to_bytes(path: &std::path::Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

pub(crate) fn spawn_walk(
    config: WalkConfig,
    sender: Sender<Result<DirEntry, WalkError>>,
    cancel: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let num_threads = config.resolved_threads().max(1);
    let injector = Arc::new(Injector::<WorkItem>::new());

    let workers: Vec<CbWorker<WorkItem>> = (0..num_threads).map(|_| CbWorker::new_lifo()).collect();
    let stealers: Arc<[Stealer<WorkItem>]> = workers
        .iter()
        .map(|w| w.stealer())
        .collect::<Vec<_>>()
        .into();

    let mut seeded = 0usize;
    for root in &config.roots {
        let root_bytes = path_to_bytes(root);
        let root_cstr = match CString::new(root_bytes.clone()) {
            Ok(c) => c,
            Err(_) => {
                let _ = sender.send(Err(WalkError {
                    path: Some(root.clone()),
                    depth: 0,
                    kind: WalkErrorKind::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path contains NUL byte",
                    )),
                }));
                continue;
            }
        };

        let fd = match hokori_sys::platform::open_dir(None, &root_cstr) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = sender.send(Err(WalkError {
                    path: Some(root.clone()),
                    depth: 0,
                    kind: map_sys_error(e),
                }));
                continue;
            }
        };

        let dot = CStr::from_bytes_with_nul(b".\0").expect("valid dot");
        let meta = match hokori_sys::platform::stat_entry(fd, dot) {
            Ok(meta) => meta,
            Err(e) => {
                hokori_sys::platform::close_fd(fd);
                let _ = sender.send(Err(WalkError {
                    path: Some(root.clone()),
                    depth: 0,
                    kind: map_sys_error(e),
                }));
                continue;
            }
        };
        hokori_sys::platform::close_fd(fd);

        injector.push(WorkItem {
            path: root_bytes,
            depth: 0,
            root_dev: meta.dev,
        });
        seeded += 1;
    }

    let pending_dirs = Arc::new(AtomicUsize::new(seeded));

    let mut joins = Vec::with_capacity(num_threads);
    for (worker_idx, worker_deque) in workers.into_iter().enumerate() {
        let sender = sender.clone();
        let cancel = cancel.clone();
        let stealers = stealers.clone();
        let injector = injector.clone();
        let pending_dirs = pending_dirs.clone();
        let follow_symlinks = config.follow_symlinks;
        let same_filesystem = config.same_filesystem;
        let max_depth = config.max_depth;

        joins.push(std::thread::spawn(move || {
            #[cfg(target_os = "linux")]
            let buf_size = hokori_sys::platform::GETDENTS_BUF_SIZE;
            #[cfg(target_os = "macos")]
            let buf_size = hokori_sys::platform::BULK_BUF_SIZE;
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let buf_size = 64 * 1024;

            let mut worker = WalkerWorker {
                local: worker_deque,
                worker_idx,
                stealers,
                injector,
                sender,
                cancel,
                pending_dirs,
                follow_symlinks,
                same_filesystem,
                max_depth,
                buf: vec![0u8; buf_size],
            };
            worker.run();
        }));
    }

    drop(sender);

    joins
}
