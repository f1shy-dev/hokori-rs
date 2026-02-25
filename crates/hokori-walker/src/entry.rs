use hokori_sys::FileType;

#[derive(Debug, Clone)]
pub struct DirEntry {
    path: Vec<u8>,
    pub depth: u16,
    pub file_type: FileType,
    pub ino: u64,
    pub apparent_size: Option<u64>,
    pub disk_usage: Option<u64>,
    pub dev: u64,
    pub nlink: Option<u32>,
}

impl DirEntry {
    pub(crate) fn from_parts(
        path: Vec<u8>,
        depth: u16,
        file_type: FileType,
        ino: u64,
        apparent_size: Option<u64>,
        disk_usage: Option<u64>,
        dev: u64,
        nlink: Option<u32>,
    ) -> Self {
        Self {
            path,
            depth,
            file_type,
            ino,
            apparent_size,
            disk_usage,
            dev,
            nlink,
        }
    }

    pub fn path_bytes(&self) -> &[u8] {
        &self.path
    }

    pub fn path(&self) -> std::path::PathBuf {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&self.path))
    }

    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Directory
    }

    pub fn is_file(&self) -> bool {
        self.file_type == FileType::File
    }
}
