use crate::{EntryMetadata, RawDirEntry, SysError};

pub fn open_dir(_parent_fd: Option<i32>, _path: &std::ffi::CStr) -> Result<i32, SysError> {
    todo!("Implement basic openat fallback for this platform")
}

pub fn read_dir_raw(
    _fd: i32,
    _buf: &mut [u8],
    _callback: &mut dyn FnMut(RawDirEntry),
) -> Result<(), SysError> {
    todo!("Implement std::fs fallback")
}

pub fn stat_entry(_dir_fd: i32, _name: &std::ffi::CStr) -> Result<EntryMetadata, SysError> {
    todo!("Implement std::fs fallback stat")
}

pub fn close_fd(_fd: i32) {}
