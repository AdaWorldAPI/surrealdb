# AdaWorldAPI ecosystem-wide version pinning

**Status:** authoritative reference.

All AdaWorldAPI repositories (`surrealdb`, `lance-graph`, `ndarray`,
`openproject-nexgen-rs`, `WoA`, …) are constrained to the following
versions. Any new dependency, feature, or fork-patch must respect them.

## The pin matrix

| Component | Pin | Notes |
|---|---|---|
| Rust toolchain | `1.95` | `rust-toolchain.toml` `channel = "1.95"` (minor only — no patch in the channel string). |
| `lance` | `=7.0.0` | Exact pin via `=` prefix. Optional dep, gated by `kv-lance` feature here. |
| `lance-index` | `=7.0.0` | Ditto. |
| `lancedb` | `=0.30.0` | Ditto. |
| `arrow-array` | `58` | Compatible 58.x line. Optional dep, gated by `kv-lance`. |
| `arrow-schema` | `58` | Ditto. |
| `datafusion` | `53` | **Transitive** — pulled in via `lance 7.0.0`. Cargo resolves to `datafusion = "53.1.0"` in `Cargo.lock`. Don't add a direct `datafusion` dep with a different pin; bumping `lance` is the only path to a different `datafusion`. |
| `ndarray` | AdaWorldAPI git fork, `default-features = false`, features `["std", "hpc-extras"]` | The `hpc-extras` feature is what activates the `crate::simd::*` polyfill (`F64x8`, `F32x16`, etc.) — runtime-dispatched to `simd_avx2.rs` / `simd_avx512.rs` / `simd_neon.rs` / `simd_amx.rs` / `simd_wasm.rs` / `simd_scalar.rs` via a `LazyLock` capability table in `ndarray::simd_caps`. |

## Where the SIMD polyfill is consumed in this repo

`ndarray::simd::F64x8` is the 8-lane portable type. Aktive Aufrufe in
`surrealdb-core`:

- `surrealdb/core/src/idx/trees/vector.rs:267` — design note
  describing the HNSW vector index built on `F64x8`
- `surrealdb/core/src/idx/trees/vector.rs:414` — CPU detection,
  cached once
- `surrealdb/core/src/idx/trees/vector.rs:421` — L2 distance kernel
- `surrealdb/core/src/idx/trees/vector.rs:444,450` — L1 (Manhattan)
- `surrealdb/core/src/idx/trees/vector.rs:469,475` — L∞ (Chebyshev)
- `surrealdb/core/src/idx/trees/vector.rs:496` — Pearson correlation

Gated by the `vector-hpc` feature flag (empty deps list — purely a
compile-time route through the SIMD code paths).

## Verifying after a dep update

```bash
# 1. Toolchain
cat rust-toolchain.toml | grep '^channel'   # must read: channel = "1.95"

# 2. Surrealdb-side pins
grep -E '^(lance|lance-index|lancedb|arrow-array|arrow-schema) *=' \
  surrealdb/core/Cargo.toml

# 3. Datafusion is transitive — check the resolved version in lockfile
grep -A1 '^name = "datafusion"$' Cargo.lock | head -2

# 4. ndarray must be the fork with hpc-extras
grep '^ndarray = ' Cargo.toml | grep AdaWorldAPI

# 5. crate::simd usage must compile (proves polyfill is wired)
cargo check -p surrealdb-core --features kv-lance,vector-hpc
```

If any of the five checks above changes shape, surface it to the human
before merging — the AdaWorldAPI ecosystem assumes these pins everywhere.

## Cross-repo correspondence

These same pins apply (with repo-relevant subset):

- `AdaWorldAPI/lance-graph` — rust 1.95, lance 7.0.0, lancedb 0.30,
  datafusion 53, arrow 58, ndarray fork
- `AdaWorldAPI/ndarray` — rust 1.95 (currently 1.94.1, slight
  diversion — separate sprint to bump), the SIMD polyfill itself
- `AdaWorldAPI/openproject-nexgen-rs` — pure Rust + path-deps; no
  direct lance/arrow/datafusion today (op-codegen pipeline emits DDL
  strings only), but the toolchain rule still applies
- `AdaWorldAPI/WoA` — Python project, only the rust toolchain rule
  applies and only for any rust subcomponents

Last verified: 2026-06-02 against fork's `main` (HEAD `0d3632f`).
