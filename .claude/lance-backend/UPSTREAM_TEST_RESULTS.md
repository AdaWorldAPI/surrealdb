# Upstream integration tests under SURREAL_TEST_KV=lance

> Generated: 2026-05-16
> Branch: `claude/phase-2-test-aggregation`
> Features: `kv-lance kv-mem`
> rustc target-cpu: `x86-64-v3` (per `.cargo/config.toml`)
> ndarray: AdaWorldAPI fork via git dep (no `[patch.crates-io]`)
>
> Each row is one `cargo test --test <name>` run. Lance datasets are
> created fresh per `#[tokio::test]` via the helpers.rs routing
> (`SURREAL_TEST_KV=lance` → `lance:///tmp/srdb-test-lance-<uuid>`).
> Each `cargo test --test <name>` run includes its own test-binary
> compile cost on cold target.

## Results

| # | Test binary | Pass / Total | Wall time | Source sprint | Notes |
|---|---|---|---|---|---|
| 1 | `create` | **3 / 3** | 49.8s | Sprint O (#1) | First end-to-end SurrealQL CREATE verification on kv-lance |
| 2 | `update` | **2 / 2** | 36.5s | Sprint S (#5) | UPDATE statements + permissions |
| 3 | `select` | **9 / 9** | 167.8s | Sprint X (pre-batch) | SELECT incl. subqueries, FETCH, ORDER, LIMIT |
| 4 | `delete` | **3 / 3** | 37.7s | Sprint X | DELETE with permissions |
| 5 | `insert` | **16 / 16** | 47.1s | Sprint X | INSERT variants, conflict semantics, RETURN |
| 6 | `upsert` | **2 / 2** | 50.1s | Sprint X | UPSERT routes through MergeInsertBuilder (Sprint N) |
| 7 | `merge` | **1 / 1** | 0.6s | Sprint X | MERGE statement |
| 8 | `relate` | **6 / 6** | 1.8s | Sprint X | Graph edge RELATE — table/record routing through lance |

**Cumulative: 42/42 across 8 upstream CRUD test binaries.**

Zero failures. Zero compile errors (the `#![recursion_limit = "1024"]`
attribute on each file, added across Sprints O / S / V, is what makes
the combined `kv-lance + kv-mem` feature set buildable).

## What this verifies end-to-end

For every test in the table above, the path through the kv-lance
backend is:

```
SurrealQL parser
    → planner
    → executor
    → Transactable::{get, set, put, putc, del, delc, scan, scanr, commit}
    → Lance MergeInsertBuilder (atomic upsert; Sprint N)
    → BTREE scalar index on `key` (Sprint M)
    → arrow-array 57 RecordBatch → disk via Lance v4.0
    → background optimizer compact_files + cleanup_old_versions (Sprint I)
```

That's the full implementation surface from Days 1–12 of
`DAY_BY_DAY.md` exercised against real SurrealQL workloads, not just
synthetic unit tests.

## Not yet verified (next sprint candidates)

The 22 other integration test binaries in `surrealdb/core/tests/`
(`access`, `alter`, `asyncevent`, `auth_limit`, `cache`, `changefeeds`,
`complex`, `define`, `field`, `function`, `future`, `index`, `info`,
`live`, `param`, `query`, `remove`, `script`, `sequence`, `table`,
`timeout`, `util`) are NOT yet exercised under `SURREAL_TEST_KV=lance`.

Each has `#![recursion_limit = "1024"]` already (Sprint V — PR #8), so
they compile. The Meta-X runs above stopped at 5 binaries to keep the
sprint bounded; a follow-up batch can pick up `define` / `query` /
`index` / `function` next (the four most-likely-to-surface-gaps).

## Failure dump

(empty — no failures in the verified set)

## Conditions

- Host CPU at `target-cpu=x86-64-v3` (AVX2 baseline).
- Per-test Lance dataset directories left in `/tmp` (filesystem reclaim is
  OS-handled).
- `tokio::test` single-threaded runtime (kv-lance integration tests
  aren't parallelism-stressed here; Sprint U's `test_concurrent_*` tests
  cover that separately, 2/2 passing).
