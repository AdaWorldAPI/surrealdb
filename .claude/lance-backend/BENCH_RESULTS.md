# BENCH_RESULTS.md — vector-hpc SIMD vs scalar distance kernels

> **Generated:** 2026-05-16
> **Bench:** `cargo bench -p surrealdb-core --features vector-hpc --bench vector_distance`
> **Host:** Intel Xeon @ 2.80 GHz; AVX-512 present (`avx512f, dq, cd, bw, vl, vnni`)
> **Compile target:** `x86-64-v3` (per `.cargo/config.toml`)
> **Toolchain:** `rustc 1.95.0`
> **Dispatch:** `ndarray::simd::F64x8` polyfill via cached `LazyLock<Tier>` —
> the host's AVX-512 codepath is selected at startup despite the v3 build
> target (cpuid runtime detect, not compile-time emit).

## Headline

**Cosine on 1024-dim vectors:** 17.9× speedup (3 590 ns → 200 ns).
**Worst-case (Euclidean, 1024-dim):** 5.2× speedup.
**All metrics, all dimensions:** SIMD is **at least 4.6×** faster than scalar.

## Full results

Each cell is criterion's mean point estimate from 100 samples × 5 s collection.

### Cosine distance (`1 - dot/(|a|·|b|)`)

| dim | scalar (ns) | simd_hpc (ns) | speedup | scalar throughput | simd throughput |
|---|---:|---:|---:|---:|---:|
| 128  | 317.4   | 34.3  | **9.25×**  | 403 Melem/s | 3 728 Melem/s |
| 384  | 1 259.3 | 77.2  | **16.31×** | 305 Melem/s | 4 974 Melem/s |
| 768  | 2 663.7 | 153.1 | **17.40×** | 288 Melem/s | 5 017 Melem/s |
| 1024 | 3 590.9 | 200.7 | **17.89×** | 285 Melem/s | 5 102 Melem/s |

### Euclidean distance (L2, `sqrt(Σ(a-b)²)`)

| dim | scalar (ns) | simd_hpc (ns) | speedup | scalar throughput | simd throughput |
|---|---:|---:|---:|---:|---:|
| 128  | 126.1   | 27.0  | **4.67×** | 1.01 Gelem/s | 4.74 Gelem/s |
| 384  | 436.1   | 78.1  | **5.59×** | 881 Melem/s  | 4.92 Gelem/s |
| 768  | 902.4   | 174.2 | **5.18×** | 851 Melem/s  | 4.41 Gelem/s |
| 1024 | 1 217.6 | 236.2 | **5.16×** | 841 Melem/s  | 4.34 Gelem/s |

### Manhattan distance (L1, `Σ|a-b|`)

| dim | scalar (ns) | simd_hpc (ns) | speedup | scalar throughput | simd throughput |
|---|---:|---:|---:|---:|---:|
| 128  | 130.8   | 22.6  | **5.79×** | 979 Melem/s | 5.66 Gelem/s |
| 384  | 446.6   | 65.0  | **6.87×** | 860 Melem/s | 5.91 Gelem/s |
| 768  | 909.3   | 124.1 | **7.33×** | 844 Melem/s | 6.19 Gelem/s |
| 1024 | 1 228.3 | 162.4 | **7.56×** | 836 Melem/s | 6.30 Gelem/s |

## Why cosine gets the largest speedup

Cosine does **three** reductions per call (dot product + two norm sums), all
SIMD-friendly accumulators that AVX-512 collapses into a few `vmulpd` +
`vfmadd231pd` + `vreducepd` per chunk. Scalar cosine has 3× the iteration
overhead. Euclidean has a final `sqrt()` (a scalar operation after the
reduction) which caps its speedup. Manhattan benefits from native
`vpabsq`-class instructions for the `abs()`.

## Throughput context

At 1024-dim cosine, the SIMD path is **5.1 Gelem/sec** = ~5 million 1024-dim
similarity comparisons per second per thread. The README claim of "611 million
cosine ops/sec at 65W on consumer CPU" is for a different metric (CAM-PQ
palette-lookup cosine, integer u8 LUT, no float math). That number is order of
magnitude higher because it doesn't do FMA arithmetic at all — it's a single
byte read per pair. The numbers in this file are for the F64x8 polyfill
processing real `f64` inputs end-to-end.

Both are in scope for surrealdb's vector index work:

- **F64x8 (this bench):** wired today via `vector-hpc` feature on
  `cosine_distance_*`, `euclidean_distance`, `manhattan_distance` in
  `surrealdb/core/src/idx/trees/vector.rs`.
- **CAM-PQ palette:** future Phase 3 work; surfaces as `lance-graph`'s
  HHTL cascade (`AdaWorldAPI/lance-graph` workspace).

## Compile target note

These numbers are with `target-cpu=x86-64-v3` (AVX2 baseline; CI-safe per
PR #9). On a true v4 host the compiler can inline AVX-512 intrinsics
directly rather than dispatching through the polyfill's `LazyLock<Tier>`
per call site, which typically buys another 10-20% on workloads dominated
by tight SIMD loops. Local builds can opt in via:

```bash
RUSTFLAGS="-C target-cpu=x86-64-v4" cargo bench \
    -p surrealdb-core --features vector-hpc \
    --bench vector_distance
```

## Reproducing

```bash
cargo bench -p surrealdb-core --features vector-hpc --bench vector_distance
# Numbers land in target/criterion/<group>/<variant>/<dim>/new/estimates.json
# and the on-disk HTML report at target/criterion/report/index.html.
```

## Variance

Outlier counts per bench point ranged from 0 to 16 (out of 100 samples). The
~16-outlier points (manhattan/scalar/128) are at the bottom of the
sub-microsecond regime where OS scheduling jitter dominates; the median +
trimmed mean stay tight. No bench point's confidence interval spans the
opposite-variant's estimate, so all reported speedups are statistically
clean.
