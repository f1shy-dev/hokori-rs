use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hokori_scan::{ScanConfig, Scanner, SizeMode};
use std::path::PathBuf;

fn bench_scan_modes(c: &mut Criterion) {
    let target = std::env::var("BENCH_TARGET").unwrap_or_else(|_| "/tmp".to_string());
    let mut group = c.benchmark_group("scan");
    group.sample_size(10);

    for (name, mode) in [
        ("disk_usage", SizeMode::DiskUsage),
        ("apparent_size", SizeMode::ApparentSize),
    ] {
        group.bench_with_input(BenchmarkId::new("scan_mode", name), &mode, |b, &mode| {
            b.iter(|| {
                let config = ScanConfig {
                    roots: vec![PathBuf::from(&target)],
                    size_mode: mode,
                    ..Default::default()
                };
                let scanner = Scanner::new(config);
                scanner.scan_blocking()
            });
        });
    }
    group.finish();
}

fn bench_scan_dedup(c: &mut Criterion) {
    let target = std::env::var("BENCH_TARGET").unwrap_or_else(|_| "/tmp".to_string());
    let mut group = c.benchmark_group("scan_dedup");
    group.sample_size(10);

    for dedup in [true, false] {
        group.bench_with_input(BenchmarkId::new("dedup", dedup), &dedup, |b, &dedup| {
            b.iter(|| {
                let config = ScanConfig {
                    roots: vec![PathBuf::from(&target)],
                    dedup_hardlinks: dedup,
                    ..Default::default()
                };
                let scanner = Scanner::new(config);
                scanner.scan_blocking()
            });
        });
    }
    group.finish();
}

fn bench_scan_threads(c: &mut Criterion) {
    let target = std::env::var("BENCH_TARGET").unwrap_or_else(|_| "/tmp".to_string());
    let mut group = c.benchmark_group("scan_threads");
    group.sample_size(10);

    for threads in [1, 2, 4, 8, 16] {
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, &threads| {
            b.iter(|| {
                let config = ScanConfig {
                    roots: vec![PathBuf::from(&target)],
                    threads,
                    ..Default::default()
                };
                let scanner = Scanner::new(config);
                scanner.scan_blocking()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_scan_modes, bench_scan_dedup, bench_scan_threads);
criterion_main!(benches);
