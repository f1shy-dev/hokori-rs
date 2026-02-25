use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hokori_walker::{WalkConfig, Walker};
use std::path::PathBuf;

fn bench_walk_tmpdir(c: &mut Criterion) {
    let mut group = c.benchmark_group("walker");
    group.sample_size(10);

    for threads in [1, 2, 4, 8] {
        group.bench_with_input(BenchmarkId::new("walk_tmp", threads), &threads, |b, &threads| {
            b.iter(|| {
                let config = WalkConfig {
                    roots: vec![PathBuf::from("/tmp")],
                    threads,
                    ..Default::default()
                };
                let walker = Walker::new(config);
                let (rx, _handle) = walker.walk();
                let mut count = 0u64;
                for _ in rx {
                    count += 1;
                }
                count
            });
        });
    }
    group.finish();
}

fn bench_walk_home(c: &mut Criterion) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let mut group = c.benchmark_group("walker_home");
    group.sample_size(10);

    group.bench_function("walk_home_default_threads", |b| {
        b.iter(|| {
            let config = WalkConfig {
                roots: vec![PathBuf::from(&home)],
                threads: 0,
                ..Default::default()
            };
            let walker = Walker::new(config);
            let (rx, _handle) = walker.walk();
            let mut count = 0u64;
            for _ in rx {
                count += 1;
            }
            count
        });
    });
    group.finish();
}

criterion_group!(benches, bench_walk_tmpdir, bench_walk_home);
criterion_main!(benches);
