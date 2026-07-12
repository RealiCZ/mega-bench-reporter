//! Canary benches — cover the two criterion shapes mega-evm uses (grouped
//! `bench_function` and grouped `bench_with_input`), so the profile parts'
//! URI shapes match what the reporter's parser sees in production.

use codspeed_canary::{fib, sum_squares};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

fn bench_fib(c: &mut Criterion) {
    let mut group = c.benchmark_group("canary_group");
    group.bench_function("fib_20", |b| b.iter(|| fib(std::hint::black_box(20))));
    group.finish();
}

fn bench_sum(c: &mut Criterion) {
    let mut group = c.benchmark_group("canary_inputs");
    for upto in [64u64, 256] {
        group.bench_with_input(BenchmarkId::new("sum_squares", upto), &upto, |b, &n| {
            b.iter(|| sum_squares(std::hint::black_box(n)))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_fib, bench_sum);
criterion_main!(benches);
