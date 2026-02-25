use hokori_scan::ScanResult;
use human_bytes::human_bytes;

pub fn render(result: &ScanResult, errors: &[impl std::fmt::Display], cli: &super::super::Cli) {
    if result.roots.len() > 1 {
        for root in &result.roots {
            println!(
                "{:>10}  {}",
                human_bytes(root.total_size as f64),
                root.path.display()
            );
        }
        println!("{}", "─".repeat(40));
    }

    println!(
        "{:>10}  total ({} files, {} dirs)",
        human_bytes(result.total_size as f64),
        format_count(result.file_count),
        format_count(result.dir_count),
    );

    if let Some(n) = cli.top.filter(|n| *n > 0) {
        if let Some(tree) = &result.tree {
            let dirs = tree.top_dirs(n, cli.apparent_size);
            println!();
            println!("Top {} directories:", n);
            for (size, path) in &dirs {
                println!("{:>10}  {}", human_bytes(*size as f64), path);
            }
        }
    }

    if !errors.is_empty() {
        eprintln!();
        for err in errors.iter().take(10) {
            eprintln!("error: {}", err);
        }
        if errors.len() > 10 {
            eprintln!("... and {} more errors", errors.len() - 10);
        }
    }
}

fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}
