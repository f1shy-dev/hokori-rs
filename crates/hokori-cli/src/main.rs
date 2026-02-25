use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;

mod output;
mod progress;

#[derive(Parser, Debug)]
#[command(
    name = "hokori",
    about = "The world's fastest disk scanner",
    version,
    long_about = "Ultra-fast disk usage analyzer built with raw syscalls and parallel work-stealing.\n\
                  Uses getdents64+statx on Linux, getattrlistbulk on macOS."
)]
pub(crate) struct Cli {
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    #[arg(short = 'f', long, default_value = "human")]
    format: OutputFormat,

    #[arg(long)]
    apparent_size: bool,

    #[arg(long)]
    count_links: bool,

    #[arg(short = 'x', long)]
    one_file_system: bool,

    #[arg(short = 'L', long)]
    follow_links: bool,

    #[arg(short = 'd', long, default_value = "0")]
    max_depth: usize,

    #[arg(short = 't', long, default_value = "0")]
    threads: usize,

    #[arg(short = 'q', long)]
    quiet: bool,

    #[arg(long)]
    stats: bool,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum OutputFormat {
    Human,
    Json,
    Ncdu,
}

fn main() {
    let cli = Cli::parse();

    let config = hokori_scan::ScanConfig {
        roots: cli.paths.clone(),
        threads: cli.threads,
        size_mode: if cli.apparent_size {
            hokori_scan::SizeMode::ApparentSize
        } else {
            hokori_scan::SizeMode::DiskUsage
        },
        dedup_hardlinks: !cli.count_links,
        follow_symlinks: cli.follow_links,
        same_filesystem: cli.one_file_system,
        max_depth: cli.max_depth,
        build_tree: matches!(cli.format, OutputFormat::Ncdu),
    };

    let scanner = hokori_scan::Scanner::new(config);
    let handle = scanner.scan();

    let show_progress = !cli.quiet && std::io::stderr().is_terminal();
    let progress_thread = if show_progress {
        Some(progress::spawn_progress_bar(handle.progress.clone()))
    } else {
        let progress_rx = handle.progress.clone();
        Some(std::thread::spawn(move || {
            for _ in progress_rx {}
        }))
    };

    let start = std::time::Instant::now();
    let (result, errors) = handle.wait();
    let elapsed = start.elapsed();

    if let Some(t) = progress_thread {
        let _ = t.join();
    }

    match cli.format {
        OutputFormat::Human => output::human::render(&result, &errors, &cli),
        OutputFormat::Json => output::json::render(&result, &errors),
        OutputFormat::Ncdu => output::ncdu::render(&result, &cli.paths),
    }

    if cli.stats {
        let elapsed_secs = elapsed.as_secs_f64();
        let throughput = if elapsed_secs > 0.0 {
            result.file_count as f64 / elapsed_secs
        } else {
            0.0
        };
        eprintln!();
        eprintln!("--- scan statistics ---");
        eprintln!("  time:       {:.3}s", elapsed_secs);
        eprintln!("  files:      {}", result.file_count);
        eprintln!("  dirs:       {}", result.dir_count);
        eprintln!("  errors:     {}", result.error_count);
        eprintln!("  deduped:    {}", result.deduped_count);
        eprintln!("  throughput: {:.0} files/sec", throughput);
        let bytes_per_sec = if elapsed_secs > 0.0 {
            result.total_size as f64 / elapsed_secs
        } else {
            0.0
        };
        eprintln!("  bandwidth:  {}/s", human_bytes::human_bytes(bytes_per_sec));
    }

    if !errors.is_empty() {
        std::process::exit(1);
    }
}
