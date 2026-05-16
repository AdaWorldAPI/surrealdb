#![cfg(feature = "vector-hpc")]
#![allow(clippy::unwrap_used)]

//! SIMD vs scalar distance kernel microbenchmarks.
//!
//! Compares the ndarray-hpc F64x8-backed kernels against straightforward
//! scalar reference implementations on representative embedding sizes
//! (128, 384, 768, 1024). The absolute timings depend on the host CPU
//! (AVX-512 vs AVX2 vs scalar fallback dispatched via the polyfill's
//! `LazyLock<Tier>` cache); the ratio is the meaningful number.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p surrealdb-core --features vector-hpc --bench vector_distance
//! ```
//!
//! For a quick smoke run (skips warmup, ~one sample group):
//!
//! ```bash
//! cargo bench -p surrealdb-core --features vector-hpc --bench vector_distance -- --quick
//! ```

use criterion::{
    BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};

/// Deterministic LCG so the inputs are bit-stable across runs. No `rand` dep.
fn lcg_vec(seed: u64, dim: usize) -> Vec<f64> {
    let mut state = seed;
    (0..dim)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
        })
        .collect()
}

// ============================================================================
//  Scalar reference kernels (inlined here to keep the bench self-contained
//  and to ensure the compiler can't accidentally route them through a
//  surrealdb-core path that has its own SIMD).
// ============================================================================

#[inline]
fn scalar_cosine_distance(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|y| y * y).sum::<f64>().sqrt();
    1.0 - dot / (na * nb)
}

#[inline]
fn scalar_euclidean_distance(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).powi(2))
        .sum::<f64>()
        .sqrt()
}

#[inline]
fn scalar_manhattan_distance(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
}

// ============================================================================
//  Bench groups
// ============================================================================

fn bench_cosine(c: &mut Criterion) {
    let mut group = c.benchmark_group("cosine_distance");
    for &dim in &[128usize, 384, 768, 1024] {
        let a = lcg_vec(0xC0FFEE, dim);
        let b = lcg_vec(0xBADBEEF, dim);

        group.throughput(Throughput::Elements(dim as u64));

        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bencher, _| {
            bencher.iter(|| scalar_cosine_distance(black_box(&a), black_box(&b)));
        });

        group.bench_with_input(BenchmarkId::new("simd_hpc", dim), &dim, |bencher, _| {
            bencher.iter(|| {
                let sim = ndarray::hpc::heel_f64x8::cosine_f64_simd(
                    black_box(&a),
                    black_box(&b),
                );
                black_box(1.0 - sim);
            });
        });
    }
    group.finish();
}

fn bench_euclidean(c: &mut Criterion) {
    use ndarray::simd::F64x8;

    /// Reproduces the Sprint P.2 kernel inline so the bench doesn't depend
    /// on `surrealdb-core` re-exporting an internal helper.
    fn simd_euclidean(a: &[f64], b: &[f64]) -> f64 {
        let n = a.len().min(b.len());
        let chunks = n / 8;
        let remainder = n % 8;
        let mut sum_sq = F64x8::splat(0.0);
        for i in 0..chunks {
            let va = F64x8::from_slice(&a[i * 8..i * 8 + 8]);
            let vb = F64x8::from_slice(&b[i * 8..i * 8 + 8]);
            let diff = va - vb;
            sum_sq = diff.mul_add(diff, sum_sq);
        }
        let mut acc = sum_sq.reduce_sum();
        let offset = chunks * 8;
        for i in 0..remainder {
            let d = a[offset + i] - b[offset + i];
            acc += d * d;
        }
        acc.sqrt()
    }

    let mut group = c.benchmark_group("euclidean_distance");
    for &dim in &[128usize, 384, 768, 1024] {
        let a = lcg_vec(0xC0FFEE, dim);
        let b = lcg_vec(0xBADBEEF, dim);
        group.throughput(Throughput::Elements(dim as u64));
        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bencher, _| {
            bencher.iter(|| scalar_euclidean_distance(black_box(&a), black_box(&b)));
        });
        group.bench_with_input(BenchmarkId::new("simd_hpc", dim), &dim, |bencher, _| {
            bencher.iter(|| simd_euclidean(black_box(&a), black_box(&b)));
        });
    }
    group.finish();
}

fn bench_manhattan(c: &mut Criterion) {
    use ndarray::simd::F64x8;

    /// Same shape as Sprint Q's `manhattan_distance_f64_simd`.
    fn simd_manhattan(a: &[f64], b: &[f64]) -> f64 {
        let n = a.len().min(b.len());
        let chunks = n / 8;
        let remainder = n % 8;
        let mut acc = F64x8::splat(0.0);
        for i in 0..chunks {
            let va = F64x8::from_slice(&a[i * 8..i * 8 + 8]);
            let vb = F64x8::from_slice(&b[i * 8..i * 8 + 8]);
            acc = acc + (va - vb).abs();
        }
        let mut sum = acc.reduce_sum();
        let offset = chunks * 8;
        for i in 0..remainder {
            sum += (a[offset + i] - b[offset + i]).abs();
        }
        sum
    }

    let mut group = c.benchmark_group("manhattan_distance");
    for &dim in &[128usize, 384, 768, 1024] {
        let a = lcg_vec(0xC0FFEE, dim);
        let b = lcg_vec(0xBADBEEF, dim);
        group.throughput(Throughput::Elements(dim as u64));
        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bencher, _| {
            bencher.iter(|| scalar_manhattan_distance(black_box(&a), black_box(&b)));
        });
        group.bench_with_input(BenchmarkId::new("simd_hpc", dim), &dim, |bencher, _| {
            bencher.iter(|| simd_manhattan(black_box(&a), black_box(&b)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_cosine, bench_euclidean, bench_manhattan);
criterion_main!(benches);
