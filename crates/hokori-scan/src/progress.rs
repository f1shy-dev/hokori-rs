use std::time::Instant;

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub bytes_scanned: u64,
    pub errors: u64,
    pub current_path: Option<String>,
    pub elapsed_secs: f64,
}

pub struct ProgressTracker {
    sender: crossbeam_channel::Sender<ScanProgress>,
    files: u64,
    dirs: u64,
    bytes: u64,
    errors: u64,
    current_path: Option<String>,
    start: Instant,
    last_update: Instant,
    update_interval_ms: u64,
}

impl ProgressTracker {
    pub fn new(sender: crossbeam_channel::Sender<ScanProgress>) -> Self {
        let now = Instant::now();
        Self {
            sender,
            files: 0,
            dirs: 0,
            bytes: 0,
            errors: 0,
            current_path: None,
            start: now,
            last_update: now,
            update_interval_ms: 100,
        }
    }

    pub fn record_file(&mut self, size: u64) {
        self.files += 1;
        self.bytes += size;
        self.maybe_send_update();
    }

    pub fn record_dir(&mut self) {
        self.dirs += 1;
        self.maybe_send_update();
    }

    pub fn record_error(&mut self) {
        self.errors += 1;
        self.maybe_send_update();
    }

    pub fn set_current_path(&mut self, path: String) {
        self.current_path = Some(path);
    }

    pub fn should_update(&self) -> bool {
        Instant::now().duration_since(self.last_update).as_millis() as u64
            >= self.update_interval_ms
    }

    fn maybe_send_update(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_update).as_millis() as u64 >= self.update_interval_ms {
            let _ = self.sender.try_send(ScanProgress {
                files_scanned: self.files,
                dirs_scanned: self.dirs,
                bytes_scanned: self.bytes,
                errors: self.errors,
                current_path: self.current_path.take(),
                elapsed_secs: now.duration_since(self.start).as_secs_f64(),
            });
            self.last_update = now;
        }
    }

    pub fn finish(&self) {
        let _ = self.sender.try_send(ScanProgress {
            files_scanned: self.files,
            dirs_scanned: self.dirs,
            bytes_scanned: self.bytes,
            errors: self.errors,
            current_path: None,
            elapsed_secs: Instant::now().duration_since(self.start).as_secs_f64(),
        });
    }
}
