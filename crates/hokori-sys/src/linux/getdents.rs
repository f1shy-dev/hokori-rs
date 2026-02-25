use crate::{FileType, RawDirEntry, SysError};
use libc::{SYS_getdents64, c_long, syscall};

#[repr(C)]
#[allow(dead_code)]
struct LinuxDirent64 {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
}

// PERF: 256KB balances syscall count vs per-thread memory. At 32 threads this is 8MB total.
// Smaller (128KB) would halve memory but roughly double syscalls for large directories.
// Larger (512KB) shows diminishing returns since most directories fit in one 256KB read.
// For spinning disks, smaller may be better (less wasted prefetch); for NVMe, 256KB is fine.
pub const GETDENTS_BUF_SIZE: usize = 256 * 1024;

// PERF: getdents64 returns d_type inline, so we avoid a stat call for files/dirs/symlinks.
// Only DT_UNKNOWN entries (rare on ext4/btrfs/xfs, common on some network filesystems)
// require a fallback stat_entry call in the walker.
const DT_UNKNOWN: u8 = 0;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

fn getdents64_raw(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { syscall(SYS_getdents64 as c_long, fd, buf.as_mut_ptr(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn d_type_to_file_type(d_type: u8) -> FileType {
    match d_type {
        DT_REG => FileType::File,
        DT_DIR => FileType::Directory,
        DT_LNK => FileType::Symlink,
        DT_UNKNOWN => FileType::Unknown,
        _ => FileType::Other,
    }
}

pub fn parse_getdents_buf(buf: &[u8], n: usize, callback: &mut dyn FnMut(RawDirEntry)) {
    let mut off = 0usize;
    while off < n {
        let base = &buf[off..n];
        if base.len() < 19 {
            break;
        }
        let d_ino = u64::from_ne_bytes(base[0..8].try_into().unwrap());
        let d_reclen = u16::from_ne_bytes(base[16..18].try_into().unwrap()) as usize;
        if d_reclen == 0 || off + d_reclen > n {
            break;
        }
        let d_type = base[18];
        let name_region = &base[19..d_reclen];
        let nul = name_region
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_region.len());
        let name = &name_region[..nul];

        if name != b"." && name != b".." {
            callback(RawDirEntry {
                // PERF: RawDirEntry keeps owning `Vec<u8>` names to avoid threading short-lived
                // borrowed lifetimes through public callback APIs across crates.
                name: name.to_vec(),
                file_type: d_type_to_file_type(d_type),
                ino: d_ino,
                size: None,
                alloc_size: None,
                dev: None,
                nlink: None,
            });
        }
        off += d_reclen;
    }
}

pub fn read_dir_raw(
    fd: i32,
    buf: &mut [u8],
    callback: &mut dyn FnMut(RawDirEntry),
) -> Result<(), SysError> {
    loop {
        let n = getdents64_raw(fd, buf)?;
        if n == 0 {
            break;
        }
        parse_getdents_buf(buf, n, callback);
    }
    Ok(())
}
