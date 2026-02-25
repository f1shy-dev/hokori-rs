use crate::SysError;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

// PERF: RESOLVE_BENEATH is only active when parent_fd is Some, which prevents
// path traversal escaping the parent directory. Currently the walker always passes
// parent_fd=None (absolute paths), so RESOLVE_BENEATH is never used. Switching to
// relative openat2(parent_fd, child_name) would enable RESOLVE_BENEATH for symlink
// safety and save path string construction, but requires tracking parent fds across
// work-stealing boundaries, complicating the design for marginal gain.
const RESOLVE_BENEATH: u64 = 0x08;

pub fn open_dir(parent_fd: Option<i32>, path: &std::ffi::CStr) -> Result<i32, SysError> {
    let dfd = parent_fd.unwrap_or(libc::AT_FDCWD);
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY) as u64,
        mode: 0,
        resolve: if parent_fd.is_some() {
            RESOLVE_BENEATH
        } else {
            0
        },
    };

    let rc = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dfd,
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };

    if rc >= 0 {
        return Ok(rc as i32);
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOSYS) {
        let rc = unsafe {
            libc::openat(
                dfd,
                path.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
        if rc < 0 {
            return Err(SysError::Io(std::io::Error::last_os_error()));
        }
        return Ok(rc);
    }

    Err(SysError::Io(err))
}

pub fn close_fd(fd: i32) {
    unsafe {
        libc::close(fd);
    }
}
