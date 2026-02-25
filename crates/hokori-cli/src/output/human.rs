use hokori_scan::ScanResult;
use hokori_scan::tree::TreeNode;
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
            let mut dirs = Vec::new();
            for root in tree {
                let mut root_path = String::from_utf8_lossy(&root.name).into_owned();
                if root_path.is_empty() {
                    root_path = "/".to_string();
                }
                collect_dirs(root, &root_path, cli.apparent_size, false, &mut dirs);
            }

            dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

            println!();
            println!("Top {} directories:", n);
            for (size, path) in dirs.into_iter().take(n) {
                println!("{:>10}  {}", human_bytes(size as f64), path);
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

fn collect_dirs(
    node: &TreeNode,
    path: &str,
    use_apparent: bool,
    include_current: bool,
    out: &mut Vec<(u64, String)>,
) {
    if node.is_dir {
        if include_current {
            let size = if use_apparent {
                node.apparent_size
            } else {
                node.disk_usage
            };
            out.push((size, path.to_string()));
        }

        for child in &node.children {
            let child_name = String::from_utf8_lossy(&child.name);
            let child_path = if path == "/" {
                format!("/{child_name}")
            } else {
                format!("{path}/{child_name}")
            };
            collect_dirs(child, &child_path, use_apparent, true, out);
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
