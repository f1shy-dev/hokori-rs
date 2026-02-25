use hokori_scan::ScanResult;
use serde::Serialize;

#[derive(Serialize)]
struct JsonOutput {
    total_size: u64,
    file_count: u64,
    dir_count: u64,
    error_count: u64,
    deduped_count: u64,
    roots: Vec<JsonRoot>,
}

#[derive(Serialize)]
struct JsonRoot {
    path: String,
    total_size: u64,
    file_count: u64,
    dir_count: u64,
}

pub fn render(result: &ScanResult, _errors: &[impl std::fmt::Display]) {
    let output = JsonOutput {
        total_size: result.total_size,
        file_count: result.file_count,
        dir_count: result.dir_count,
        error_count: result.error_count,
        deduped_count: result.deduped_count,
        roots: result
            .roots
            .iter()
            .map(|r| JsonRoot {
                path: r.path.display().to_string(),
                total_size: r.total_size,
                file_count: r.file_count,
                dir_count: r.dir_count,
            })
            .collect(),
    };
    println!("{}", serde_json::to_string(&output).unwrap());
}
