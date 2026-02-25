use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hokori_scan::dedup::InodeDedup;

fn bench_dedup_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("dedup");

    for count in [1_000, 10_000, 100_000, 1_000_000] {
        group.bench_with_input(BenchmarkId::new("insert", count), &count, |b, &count| {
            b.iter(|| {
                let dedup = InodeDedup::new();
                for i in 0..count {
                    dedup.check_and_insert(1, i);
                }
            });
        });
    }
    group.finish();
}

fn bench_dedup_collision(c: &mut Criterion) {
    let mut group = c.benchmark_group("dedup_collision");

    group.bench_function("50pct_collision", |b| {
        b.iter(|| {
            let dedup = InodeDedup::new();
            for i in 0..50_000u64 {
                dedup.check_and_insert(1, i);
            }
            for i in 0..50_000u64 {
                dedup.check_and_insert(1, i);
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_dedup_insert, bench_dedup_collision);
criterion_main!(benches);
