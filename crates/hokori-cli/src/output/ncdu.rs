use hokori_scan::ScanResult;
use std::io::{BufWriter, Write};

pub fn render(result: &ScanResult, roots: &[std::path::PathBuf]) {
    let stdout = std::io::stdout();
    let mut w = BufWriter::with_capacity(64 * 1024, stdout.lock());

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    write!(
        w,
        "[1,2,{{\"progname\":\"hokori\",\"progver\":\"{}\",\"timestamp\":{}}}",
        env!("CARGO_PKG_VERSION"),
        timestamp
    )
    .unwrap();

    if let Some(ref tree) = result.tree {
        for &root_idx in &tree.root_indices {
            write!(w, ",").unwrap();
            tree.write_ncdu_node(&mut w, root_idx).unwrap();
        }
    } else {
        for root in roots {
            let name = serde_json::to_string(&root.display().to_string()).unwrap();
            write!(
                w,
                ",[{{\"name\":{},\"asize\":{},\"dsize\":{}}}]",
                name, result.total_size, result.total_size
            )
            .unwrap();
        }
        eprintln!("note: full ncdu tree export requires --build-tree");
    }

    writeln!(w, "]").unwrap();
    w.flush().unwrap();
}
