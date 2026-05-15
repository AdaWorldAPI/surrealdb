# KNOWN_DIFFERENCES.md — kv-lance backend

> Status after Sprints A–K (Days 1–12 of DAY_BY_DAY.md).
> Captures: what works, what's deferred, semantic differences vs
> other surrealdb backends (RocksDB, SurrealKv, Mem).

## Test coverage
- `surrealdb/core/src/kvs/lance/tests.rs`: 37 unit tests pinning the
  Transactable contract end-to-end (open/close, get, set/commit,
  put/putc/del/delc, scan/scanr/keys/keysr, savepoints, versioning,
  background optimizer, property test).
- `surrealdb/core/src/kvs/lance/integration_tests.rs`: 3 SurrealQL-level
  smoke tests (CREATE/SELECT, UPDATE overwrite, DELETE).
- Run with: `cargo test --features "kv-lance kv-mem" --no-default-features --lib kvs::lance`

## Days completed (1–12)
- [x] Day 1 — Datastore::new opens or creates a Lance dataset.
- [x] Day 2 — Transaction::get with pending-buffer RYW + Lance scan fall-through.
- [x] Day 3 — Transaction::commit (append + delete; Sprint F fix for overwrite).
- [x] Day 4 — put / putc.
- [x] Day 5 — del / delc.
- [x] Day 6 — scan / scanr.
- [x] Day 7 — keys / keysr.
- [x] Day 8 — savepoints (pending-buffer snapshot stack).
- [x] Day 9 — versioning via Dataset::checkout_version.
- [x] Day 10 — background optimizer (compact_files + cleanup_old_versions).
- [x] Day 11 — From<lance::Error> for Error + property test.
- [x] Day 12 — Higher-level integration smoke tests (this sprint).

## Lance crate version
Pinned to `lance = "1.0"` (resolves to 1.0.4 at sprint time). Verify with `cargo tree -p lance`.

## Architectural decisions / deviations from the scaffold's TODO comments

### Arrow version split (v55 vs v56)
Cargo.toml pins `arrow-array = "55"`. lance 1.0.4 internally uses arrow-array v56.
The two versions have distinct type IDs and cannot be mixed at the Rust type level.
- `KvSchema::build_write_batch` (schema.rs) still uses our v55 types for its unit
  tests and for any caller that wants the smaller dep tree.
- A private helper `Transaction::build_write_batch_lance` in mod.rs builds the
  RecordBatch with `lance::deps::arrow_array` (v56) types for the actual
  `Dataset::append` call.
- All Scanner downcasts use `lance::deps::arrow_array::BinaryArray`.

### Commit semantics (Sprint F P0 fix)
Lance is append-only. A naive commit() that just appends new rows leaves multiple
rows for the same key — `Transaction::get` (scan + limit 1) then returns one
non-deterministically. Sprint F fixed this by issuing a Dataset::delete (over
the keys being written) BEFORE the Dataset::append. Net effect: each key has at
most one row after commit.

This means **version-snapshot reads at an older version may NOT see a value that
was overwritten at a later version**, because the delete-at-version-N marks the
row deleted in Lance's deletion vectors. This is acceptable for KV semantics but
worth noting for time-travel use cases.

### No atomic append+delete
lance 1.0.4 does not expose a public `with_transaction` API. commit() therefore
issues `Dataset::delete` then `Dataset::append` sequentially. In a crash between
the two, the dataset would be partially updated. Lance's OCC + commit-log
catches some classes of conflict but not all. For BindSpace-prefix-sharded
workloads the practical impact is negligible.

### ScanLimit::Bytes fallback
ScanLimit has Count(u32), Bytes(u32), BytesOrCount(u32, u32). The lance
scan_impl honors Count and the count side of BytesOrCount. Bytes alone falls
back to `take(10_000)` — a generous count cap rather than actual byte
accounting. Properly honoring byte size requires per-row size computation
during the merge step; deferred.

### BTREE scalar index on `key` not yet wired
B1 noted that `lance::index::IndexType` + `ScalarIndexParams` are not
re-exported by the lance 1.0 public API. Adding `lance-index = "=1.0.4"` to
Cargo.toml would unlock this. Without the index, point lookups still work
(linear scan with filter pushdown via DataFusion) but are O(N) rather than
O(log N). For datasets > ~100k rows this matters; for POC traffic, irrelevant.

### Background optimizer module path
`Dataset::compact_files` is a free function in `lance::dataset::optimize`, NOT
a method on `Dataset`. `cleanup_old_versions` IS a Dataset method and takes
`chrono::Duration` (not `std::time::Duration`).

### Scanner builder pattern
Lance 1.0.4 Scanner methods (`filter`, `project`, `limit`, `order_by`) return
`Result<&mut Self>`, not `Result<Self>`. They cannot be fluently chained via
`?`; must be called sequentially on a `let mut scanner = ...` binding.

### Dataset::checkout name
Day-2's get implementation found that the API is `Dataset::checkout_version(impl Into<Ref>)`,
not `Dataset::checkout(u64)` as the original scaffold suggested.

### ColumnOrdering API
`Scanner::order_by` takes `Option<Vec<ColumnOrdering>>`. Direction baking is
done via constructors `ColumnOrdering::asc_nulls_first(String)` and
`ColumnOrdering::desc_nulls_first(String)`. There is no `.with_ascending()`
method — direction is fixed at construction time.

### threadpool module gate
`surrealdb/core/src/kvs/threadpool.rs` carries a file-level
`#![cfg(any(feature = "kv-mem", feature = "kv-rocksdb", feature = "kv-surrealkv"))]`
gate that excludes `kv-lance`. The ds.rs URL arm must NOT call
`super::threadpool::initialise()`. Lance owns its own async runtime.

### Box<dyn Transactable> coercion
Unlike `surrealkv` and `mem` (which return `Result<Box<dyn Transactable>>`
directly from their `transaction()` methods), `lance::Datastore::transaction()`
returns `Result<Transaction>` (concrete type). The ds.rs dispatch arm wraps it:
`Box::new(tx) as Box<dyn Transactable>`.

### Error variant additions (Sprint A4 / J1)
- `kvs::Error::NoSavePointPresent` added as a new unit variant (no upstream
  equivalent; tikv used `Error::Transaction("No savepoint...")` string-based).
- `impl From<lance::Error> for Error` maps `CommitConflict` and
  `RetryableCommitConflict` to `Error::TransactionConflict(String)` (which has
  `is_retryable() = true`). `DatasetNotFound` maps to `Error::Datastore(...)`.
  All other lance error variants fall through to `Error::Datastore(format!("lance: {other}"))`.
- `lance::Error::TransactionRetryable` does NOT exist as a distinct variant in
  kvs::Error; `TransactionConflict` is reused (semantically equivalent because
  its `is_retryable()` impl returns true).

### BooleanArray construction (Sprint A2)
The scaffold's `iter::repeat(false/true).take(n).collect::<BooleanArray>()` did
not compile because `FromIterator<bool>` is not implemented for `BooleanArray`
in arrow-array v55. Fixed by `BooleanArray::from(vec![false/true; n])`.

## Semantic differences vs RocksDB/SurrealKv backends

| Aspect | RocksDB / SurrealKv | kv-lance |
| --- | --- | --- |
| Storage model | LSM tree / B-tree (row-store, point-lookup-optimised) | Columnar (Arrow) with deletion vectors |
| MVCC | Snapshot per txn, internal | Native dataset versioning via Lance commit log |
| Point lookup | O(log n) BTREE | O(N) until BTREE scalar index is wired (deferred) |
| Range scan | Iterator-based, low memory | Materialized stream + in-memory BTreeMap merge |
| Overwrite | In-place write (B-tree) or LSM compaction handles dedup | Delete-before-append (Sprint F fix) |
| Atomic txn | Native | Sequential delete + append; partial commits possible mid-fault |
| Background ops | Compaction is automatic | Periodic `compact_files` + `cleanup_old_versions` task |
| Disk format | RocksDB / SurrealKv proprietary | Lance columnar, interoperable with DataFusion / Polars / pandas |
| threadpool | Shared `kvs::threadpool` | Lance owns its own async runtime; threadpool not used |

## Open / deferred (not blocking POC)

- [ ] Add `lance-index = "=1.0.4"` to Cargo.toml and wire BTREE scalar index in `Datastore::new`.
- [ ] Unify arrow-array version: either bump our pin to v56 to match lance, or migrate `KvSchema::build_write_batch` callers to use `lance::deps::arrow_array` everywhere (eliminating the dual-type-tree).
- [ ] Replace the `Bytes` ScanLimit fallback with real byte-size accounting.
- [ ] Property test with multiple concurrent transactions (current test is sequential).
- [ ] Wire the kv-lance backend into the upstream test harness via an env-var on `helpers.rs::new_ds` so the full surrealdb test suite can run against lance.
- [ ] Benchmark vs RocksDB on representative workloads (Day 12 of the original plan calls for this).
- [ ] Verify `lazy_env_parse!` macro remains accessible in cnf.rs without explicit `use` import as upstream crate root API evolves.
- [ ] Document to operators: `error_if_tagged_old_versions = Some(false)` means tagged user-snapshot versions are silently skipped (not errored) during cleanup — this is intentional but non-obvious.
