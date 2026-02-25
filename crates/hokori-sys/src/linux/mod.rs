mod getdents;
mod openat2;
mod statx;

pub use getdents::{GETDENTS_BUF_SIZE, parse_getdents_buf, read_dir_raw};
pub use openat2::{close_fd, open_dir};
pub use statx::statx_entry;

use crate::{EntryMetadata, SysError};

pub fn stat_entry(dir_fd: i32, name: &std::ffi::CStr) -> Result<EntryMetadata, SysError> {
    statx_entry(dir_fd, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileType;

    #[test]
    fn test_read_current_dir() {
        let path = std::ffi::CString::new(".").expect("valid cstring");
        let fd = open_dir(None, &path).expect("open current directory");
        let mut buf = vec![0u8; GETDENTS_BUF_SIZE];
        let mut entries = Vec::new();
        read_dir_raw(fd, &mut buf, &mut |entry| {
            entries.push(entry);
        })
        .expect("read dir entries");
        close_fd(fd);
        assert!(!entries.is_empty());
    }

    #[test]
    fn test_statx_regular_file() {
        let path = std::ffi::CString::new(".").expect("valid cstring");
        let fd = open_dir(None, &path).expect("open current directory");
        let name = std::ffi::CString::new("Cargo.toml").expect("valid cstring");
        let meta = stat_entry(fd, &name).expect("stat Cargo.toml");
        assert_eq!(meta.file_type, FileType::File);
        assert!(meta.size > 0);
        close_fd(fd);
    }
}
