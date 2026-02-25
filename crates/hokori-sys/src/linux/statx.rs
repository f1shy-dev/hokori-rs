use crate::{EntryMetadata, FileType, SysError};
use libc::{SYS_statx, c_long, syscall};

const AT_STATX_DONT_SYNC: i32 = 0x2000;

fn statx_mode_to_file_type(mode: u16) -> FileType {
    match mode & libc::S_IFMT as u16 {
        m if m == libc::S_IFREG as u16 => FileType::File,
        m if m == libc::S_IFDIR as u16 => FileType::Directory,
        m if m == libc::S_IFLNK as u16 => FileType::Symlink,
        _ => FileType::Other,
    }
}

pub fn statx_entry(dir_fd: i32, name: &std::ffi::CStr) -> Result<EntryMetadata, SysError> {
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    let flags = libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC;
    let mask = libc::STATX_TYPE
        | libc::STATX_SIZE
        | libc::STATX_BLOCKS
        | libc::STATX_INO
        | libc::STATX_NLINK;
    let rc = unsafe {
        syscall(
            SYS_statx as c_long,
            dir_fd,
            name.as_ptr(),
            flags,
            mask,
            &mut stx as *mut libc::statx,
        )
    };
    if rc < 0 {
        return Err(SysError::Io(std::io::Error::last_os_error()));
    }

    Ok(EntryMetadata {
        ino: stx.stx_ino,
        dev: ((stx.stx_dev_major as u64) << 32) | stx.stx_dev_minor as u64,
        size: stx.stx_size,
        blocks: stx.stx_blocks,
        alloc_size: stx.stx_blocks * 512,
        file_type: statx_mode_to_file_type(stx.stx_mode),
        nlink: stx.stx_nlink,
    })
}
