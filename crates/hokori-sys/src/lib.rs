//! hokori-sys: Raw platform syscall bindings for filesystem traversal.
//!
//! Provides zero-overhead access to:
//! - Linux: getdents64, statx, openat2
//! - macOS: getattrlistbulk
//! - Fallback: std::fs wrappers for other Unix

#[derive(Debug, Clone)]
pub struct RawDirEntry {
    pub name: Vec<u8>,
    pub file_type: FileType,
    pub ino: u64,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Symlink,
    Unknown,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct EntryMetadata {
    pub ino: u64,
    pub dev: u64,
    pub size: u64,
    pub blocks: u64,
    pub alloc_size: u64,
    pub file_type: FileType,
    pub nlink: u32,
}

#[derive(Debug)]
pub enum SysError {
    Io(std::io::Error),
    NeedsStat,
    BufferTooSmall,
}

impl From<std::io::Error> for SysError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub mod fallback;

#[cfg(target_os = "linux")]
pub use linux as platform;

#[cfg(target_os = "macos")]
pub use macos as platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use fallback as platform;
