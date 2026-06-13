use cerebro::activation::{actr_activation, retrievability};
use chrono::{Duration, Utc};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_actr(c: &mut Criterion) {
    let now   = Utc::now();
    let times: Vec<_> = (0..50).map(|i| now - Duration::seconds(i * 3600)).collect();

    c.bench_function("actr_50_timestamps", |b| {
        b.iter(|| actr_activation(black_box(&times), black_box(now)))
    });
}

fn bench_fsrs(c: &mut Criterion) {
    // retrievability(elapsed_days, stability)
    c.bench_function("fsrs_retrievability", |b| {
        b.iter(|| retrievability(black_box(3.0), black_box(2.0)))
    });
}

criterion_group!(benches, bench_actr, bench_fsrs);
criterion_main!(benches);
