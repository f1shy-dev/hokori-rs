use std::path::PathBuf;

#[derive(Debug)]
pub struct WalkError {
    pub path: Option<PathBuf>,
    pub depth: u16,
    pub kind: WalkErrorKind,
}

#[derive(Debug)]
pub enum WalkErrorKind {
    PermissionDenied,
    Io(std::io::Error),
    TooManyOpenFiles,
    SymlinkLoop,
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{}: {:?}", p.display(), self.kind),
            None => write!(f, "<unknown>: {:?}", self.kind),
        }
    }
}

impl std::error::Error for WalkError {}
