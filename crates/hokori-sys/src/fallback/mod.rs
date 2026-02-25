use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use crate::{EntryMetadata, FileType, RawDirEntry, SysError};

static FD_REGISTRY: LazyLock<Mutex<FdRegistry>> = LazyLock::new(|| {
    Mutex::new(FdRegistry {
        map: HashMap::new(),
        next_fd: 1000,
    })
});

struct FdRegistry {
    map: HashMap<i32, PathBuf>,
    next_fd: i32,
}

pub fn open_dir(parent_fd: Option<i32>, path: &CStr) -> Result<i32, SysError> {
    let path_buf = PathBuf::from(OsStr::from_bytes(path.to_bytes()));

    let resolved_path = if path_buf.is_absolute() || parent_fd.is_none() {
        path_buf
    } else {
        let parent = parent_fd.expect("checked is_some");
        let registry = FD_REGISTRY.lock().unwrap();
        let parent_path = registry.map.get(&parent).cloned().ok_or_else(|| {
            SysError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid fd",
            ))
        })?;
        parent_path.join(path_buf)
    };

    let meta = std::fs::symlink_metadata(&resolved_path).map_err(SysError::Io)?;
    if !meta.is_dir() {
        return Err(SysError::Io(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "not a directory",
        )));
    }

    let mut registry = FD_REGISTRY.lock().unwrap();
    let fd = registry.next_fd;
    registry.next_fd += 1;
    registry.map.insert(fd, resolved_path);
    Ok(fd)
}

pub fn read_dir_raw(
    fd: i32,
    _buf: &mut [u8],
    callback: &mut dyn FnMut(RawDirEntry),
) -> Result<(), SysError> {
    let dir_path = {
        let registry = FD_REGISTRY.lock().unwrap();
        registry.map.get(&fd).cloned().ok_or_else(|| {
            SysError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid fd",
            ))
        })?
    };

    let read_dir = std::fs::read_dir(&dir_path).map_err(SysError::Io)?;

    for entry_result in read_dir {
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name();
        let name_bytes = name.as_bytes().to_vec();

        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let metadata = entry.metadata().ok();
        let file_type = match entry.file_type() {
            Ok(ft) => {
                if ft.is_dir() {
                    FileType::Directory
                } else if ft.is_file() {
                    FileType::File
                } else if ft.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::Other
                }
            }
            Err(_) => FileType::Unknown,
        };

        callback(RawDirEntry {
            name: name_bytes,
            file_type,
            ino: metadata.as_ref().map(|m| m.ino()).unwrap_or(0),
            size: metadata.as_ref().map(|m| m.len()),
        });
    }

    Ok(())
}

pub fn stat_entry(dir_fd: i32, name: &CStr) -> Result<EntryMetadata, SysError> {
    let full_path = {
        let registry = FD_REGISTRY.lock().unwrap();
        let dir_path = registry.map.get(&dir_fd).cloned().ok_or_else(|| {
            SysError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid fd",
            ))
        })?;

        let name_bytes = name.to_bytes();
        if name_bytes == b"." {
            dir_path
        } else {
            dir_path.join(OsStr::from_bytes(name_bytes))
        }
    };

    let meta = std::fs::symlink_metadata(&full_path).map_err(SysError::Io)?;
    let kind = meta.file_type();
    let file_type = if kind.is_dir() {
        FileType::Directory
    } else if kind.is_file() {
        FileType::File
    } else if kind.is_symlink() {
        FileType::Symlink
    } else {
        FileType::Other
    };

    Ok(EntryMetadata {
        ino: meta.ino(),
        dev: meta.dev(),
        size: meta.len(),
        blocks: meta.blocks(),
        alloc_size: meta.blocks() * 512,
        file_type,
        nlink: meta.nlink() as u32,
    })
}

pub fn close_fd(fd: i32) {
    let mut registry = FD_REGISTRY.lock().unwrap();
    registry.map.remove(&fd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fallback_roundtrip() {
        let path = CStr::from_bytes_with_nul(b".\0").unwrap();
        let fd = open_dir(None, path).unwrap();

        let mut entries = Vec::new();
        let mut buf = vec![0u8; 4096];
        read_dir_raw(fd, &mut buf, &mut |e| entries.push(e)).unwrap();
        assert!(!entries.is_empty());

        for e in &entries {
            assert_ne!(e.name, b".");
            assert_ne!(e.name, b"..");
        }

        let dot = CStr::from_bytes_with_nul(b".\0").unwrap();
        let meta = stat_entry(fd, dot).unwrap();
        assert_eq!(meta.file_type, FileType::Directory);

        close_fd(fd);
    }
}
