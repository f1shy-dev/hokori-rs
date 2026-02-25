use human_bytes::human_bytes;
use indicatif::{ProgressBar, ProgressStyle};

pub fn spawn_progress_bar<I>(rx: I) -> std::thread::JoinHandle<()>
where
    I: IntoIterator<Item = hokori_scan::ScanProgress> + Send + 'static,
    I::IntoIter: Send + 'static,
{
    std::thread::spawn(move || {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );

        for progress in rx {
            pb.set_message(format!(
                "Scanning... {} files, {} dirs, {} ({:.1}s)",
                format_count(progress.files_scanned),
                format_count(progress.dirs_scanned),
                human_bytes(progress.bytes_scanned as f64),
                progress.elapsed_secs,
            ));
            pb.tick();
        }

        pb.finish_and_clear();
    })
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
