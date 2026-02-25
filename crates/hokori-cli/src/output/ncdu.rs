use hokori_scan::ScanResult;

pub fn render(result: &ScanResult, roots: &[std::path::PathBuf]) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    print!(
        "[1,2,{{\"progname\":\"hokori\",\"progver\":\"{}\",\"timestamp\":{}}}",
        env!("CARGO_PKG_VERSION"),
        timestamp
    );

    for root in roots {
        print!(
            ",[{{\"name\":\"{}\",\"asize\":{},\"dsize\":{}}}]",
            root.display(),
            result.total_size,
            result.total_size,
        );
    }
    println!("]");

    eprintln!("note: full ncdu tree export requires --build-tree (not yet implemented)");
}
