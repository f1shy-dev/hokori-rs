mod getattrlistbulk;

pub use getattrlistbulk::{BULK_BUF_SIZE, read_dir_raw};

use crate::{EntryMetadata, FileType, SysError};

fn mode_to_file_type(mode: libc::mode_t) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFREG => FileType::File,
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        _ => FileType::Other,
    }
}

pub fn open_dir(parent_fd: Option<i32>, path: &std::ffi::CStr) -> Result<i32, SysError> {
    let dfd = parent_fd.unwrap_or(libc::AT_FDCWD);
    let rc = unsafe {
        libc::openat(
            dfd,
            path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if rc < 0 {
        Err(SysError::Io(std::io::Error::last_os_error()))
    } else {
        Ok(rc)
    }
}

pub fn stat_entry(dir_fd: i32, name: &std::ffi::CStr) -> Result<EntryMetadata, SysError> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dir_fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc < 0 {
        return Err(SysError::Io(std::io::Error::last_os_error()));
    }

    Ok(EntryMetadata {
        ino: st.st_ino,
        dev: st.st_dev as u64,
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        alloc_size: st.st_blocks as u64 * 512,
        file_type: mode_to_file_type(st.st_mode),
        nlink: st.st_nlink as u32,
    })
}

pub fn close_fd(fd: i32) {
    unsafe {
        libc::close(fd);
    }
}
