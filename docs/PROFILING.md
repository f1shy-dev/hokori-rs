# Profiling hokori-rs

## CPU flamegraphs
Install the tooling once, then run on a representative large directory.
`cargo install flamegraph`
`cargo flamegraph -p hokori-cli -- /large/dir`
Use `--profile profiling` for fuller symbols when needed: `cargo flamegraph --profile profiling -p hokori-cli -- /large/dir`.

## Heap profiling (dhat)
Build with the opt-in heap profiler feature and run the normal CLI path.
`cargo run --release --features dhat-heap -p hokori-cli -- /large/dir`
This writes `dhat-heap.json` in the working directory.
Open it in Firefox's DHAT viewer for allocation hotspots.

## Phase timing
Use built-in phase timing to split walk, tree build, and output rendering.
`cargo run --release -p hokori-cli -- --timings /large/dir`
Timing output is printed to stderr, so JSON/NCDU stdout output stays machine-readable.
Use this first to identify whether traversal, aggregation, or rendering is the bottleneck.

## Linux perf
Collect coarse CPU/cache/syscall counters for quick regressions.
`perf stat cargo run --release -p hokori-cli -- /large/dir`
Compare before/after changes using the same dataset and warm/cold cache conditions.
Pair with flamegraphs once a regression is confirmed.

## Criterion benchmarks
Run the existing benchmark suite for stable micro/macro comparisons.
`cargo bench`
Use Criterion reports to inspect variance and trendlines across runs.
Prefer bench results for PR-level performance claims.
