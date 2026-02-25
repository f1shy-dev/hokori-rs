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
        let name = serde_json::to_string(&root.display().to_string()).unwrap();
        print!(
            ",[{{\"name\":{},\"asize\":{},\"dsize\":{}}}]",
            name,
            result.total_size,
            result.total_size,
        );
    }
    println!("]");

    eprintln!("note: full ncdu tree export requires --build-tree (not yet implemented)");
}
