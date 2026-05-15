# SurrealDB Lance Backend (`kv-lance`)

A storage backend for SurrealDB built on top of the [Lance columnar
format](https://lance.org). Provides ACID transactions, MVCC,
optimistic concurrency control, native dataset versioning, and
columnar storage that interoperates with [DataFusion],
[lance-graph](https://github.com/AdaWorldAPI/lance-graph), and the
HPC-tuned [ndarray fork](https://github.com/AdaWorldAPI/ndarray).

> **Status:** scaffold / POC. The trait stubs compile structurally;
> the actual Lance API calls are marked `TODO(lance-integration)` in
> each method. Pick them off one at a time — each is documented inline.

## Why?

Most SurrealDB storage backends (RocksDB, TiKV, FoundationDB,
SurrealKV) are LSM-tree or B-tree KV stores optimised for OLTP. Lance
is a versioned columnar Arrow-on-disk format optimised for OLAP and
ML workloads. The combination gives SurrealDB:

- **Native dataset versioning** — `Dataset::checkout(version)` maps
  directly to SurrealDB's `version: Option<u64>` parameter, giving
  time-travel reads for free.
- **Columnar analytics on OLTP data** — analytical queries via
  DataFusion run directly on the same storage, no CDC replica needed.
- **Branch-per-tenant workflows** — Lance's git-style branching maps
  cleanly to SurrealDB namespaces, enabling per-tenant data isolation
  with shared base data.
- **HPC vector operations** — when paired with the `ndarray` fork,
  vector-index lookups use integer-table-lookup cosine similarity at
  611M ops/sec on consumer CPUs — beating FAISS-GPU.

## File Map

```
surrealdb/core/src/kvs/lance/
├── mod.rs                       Datastore + Transaction structs + Transactable impl
├── schema.rs                    Arrow KV schema (key, val, version, tombstone)
├── cnf.rs                       SURREAL_LANCE_* config constants
├── tx_buffer.rs                 Pending-writes buffer for in-flight transactions
└── background_optimizer.rs      Periodic Dataset::optimize() task
```

## Architecture

```
┌─────────────────────────────────────────────────┐
│  SurrealQL Frontend (parser, planner, etc.)      │
└─────────────────┬───────────────────────────────┘
                  │
┌─────────────────────────────────────────────────┐
│  SurrealDB Core (transactions, indexing, kvs)    │
└─────────────────┬───────────────────────────────┘
                  │ Transactable trait
                  ▼
┌─────────────────────────────────────────────────┐
│  kvs/lance/ (this crate)                         │
│    Datastore   ── 1 Dataset per Datastore        │
│    Transaction ── pending-buffer + commit-batch  │
└─────────────────┬───────────────────────────────┘
                  │ lance::Dataset API
                  ▼
┌─────────────────────────────────────────────────┐
│  Lance: MVCC + OCC + Scalar Indexes              │
│    BTREE on `key` column for O(log n) lookup     │
│    Native versioning + git-style branching       │
└─────────────────────────────────────────────────┘
```

## KV Schema

The simplest viable schema: opaque binary keys/values with version
and tombstone columns for MVCC. A BTREE scalar index on `key` gives
O(log n) point lookups.

| Column     | Type                  | Notes                                |
| ---------- | --------------------- | ------------------------------------ |
| `key`      | `Binary`              | SurrealDB key; BTREE indexed.        |
| `val`      | `Binary`              | SurrealDB value.                     |
| `version`  | `UInt64`              | Dataset version at write time.       |
| `tombstone`| `Boolean`             | Deletion marker for MVCC reads.      |

Future optimisation: parse the SurrealDB key prefix
(`namespace|database|table|...`) into typed sub-columns to enable
column pruning. POC keeps it opaque.

## Transaction Model

Unlike SurrealKV (whose underlying `surrealkv::Tree` has an in-tree
transactional MemTable), Lance has no per-row write buffer. Pending
writes/deletes are therefore accumulated in
[`tx_buffer::PendingBuffer`] and flushed atomically in
[`Transaction::commit`]:

```rust
async fn commit(&self) -> Result<()> {
    let (writes, deletes) = self.pending.read().await.partition();
    let batch = KvSchema::build_write_batch(&writes, version)?;
    let predicate = KvSchema::build_delete_predicate(&deletes);
    let mut ds = self.dataset.write().await;
    ds.inner.with_transaction(|tx| async {
        tx.append(batch).await?;
        tx.delete(&predicate).await?;
        Ok(())
    }).await?;
    Ok(())
}
```

This is functionally equivalent to RocksDB's WriteBatch — all writes
in a transaction are visible-or-not-visible atomically.

## Concurrency

Lance provides Optimistic Concurrency Control (OCC) at the dataset
level. If two transactions modify the same row, one commits cleanly
and the other returns a retryable conflict error.

For SurrealDB workloads that need higher concurrent-write throughput,
pair this backend with **BindSpace-aware sharding** (see
[lance-graph](https://github.com/AdaWorldAPI/lance-graph)): writes
target deterministic key-prefix buckets, making collision probability
negligible at typical request rates.

## Build & Test

```bash
# Enable the feature
cargo build --features kv-lance

# Run unit tests (schema, tx_buffer)
cargo test -p surrealdb-core --features kv-lance kvs::lance

# Run SurrealDB's integration test suite against the Lance backend
SURREAL_TEST_KV=lance cargo test --features kv-lance
```

## Config (environment variables)

| Variable                                       | Type     | Default | Purpose                                 |
| ---------------------------------------------- | -------- | ------- | --------------------------------------- |
| `SURREAL_LANCE_BACKGROUND_OPTIMIZE_ENABLED`    | bool     | true    | Spawn the background optimizer task.    |
| `SURREAL_LANCE_OPTIMIZE_AFTER_N_WRITES`        | u64      | 1000    | Write-count threshold for optimize.     |
| `SURREAL_LANCE_OPTIMIZE_INTERVAL`              | duration | 5min    | Time threshold for optimize.            |
| `SURREAL_LANCE_VERSION_RETENTION_SECS`         | u64      | 7d      | Age threshold for version pruning.      |
| `SURREAL_LANCE_DELETE_VIA_TOMBSTONE_ROW`       | bool     | false   | Write explicit tombstone rows.          |
| `SURREAL_LANCE_CREATE_KEY_INDEX_ON_OPEN`       | bool     | true    | Auto-create BTREE index at open.        |
| `SURREAL_LANCE_COMMIT_MAX_BATCH_ROWS`          | usize    | 10_000  | Split very large commits into chunks.   |

## Roadmap

### Phase 1 — POC (this scaffold + Lance wiring)

- [x] Module scaffold with `Transactable` trait skeleton
- [x] Arrow KV schema
- [x] Pending-writes buffer with savepoint support
- [x] Background optimizer task
- [ ] Wire `lance::Dataset` into `DatasetHandle`
- [ ] Implement `get` / `set` via `Dataset::scan().filter` + `Dataset::append`
- [ ] Implement `del` / `commit` with combined append+delete transaction
- [ ] Implement `keys` / `scan` range operations
- [ ] Implement `getp` / `delp` prefix operations (default-impl review)
- [ ] Wire `Datastore::current_version` to Lance's `Dataset::version`
- [ ] Pass SurrealDB's `tests/` suite end-to-end

### Phase 2 — Performance

- [ ] Replace the HashMap-based `PendingBuffer` with a `BTreeMap` if
      profiling shows scan-merge dominates commit time
- [ ] Add bulk-load mode (skip indexing during initial dataset
      bootstrap, then create index once)
- [ ] Integrate `ndarray` cosine-similarity for SurrealDB's vector
      index type
- [ ] Multi-bucket sharding (one Lance dataset per BindSpace bucket)
      for write-throughput scaling

### Phase 3 — Differentiation

- [ ] Expose Lance's branch/tag operations as `DEFINE BRANCH` /
      `DEFINE TAG` SurrealQL statements
- [ ] Bridge `lance-graph` (Cypher engine) so SurrealDB tables are
      queryable as graph datasets
- [ ] Bridge `blasgraph` (GraphBLAS) so Cypher-via-SurrealDB can
      compile to sparse-matrix algebra for analytic queries
- [ ] Hook `lance-graph-ontology` as a SurrealDB function provider
      for OGIT-style reasoning

## License

Same as parent SurrealDB project (BSL 1.1, converting to Apache 2.0
on 2030-01-01). Note: this backend depends on `lance` (Apache 2.0)
and `arrow-rs` (Apache 2.0).

[DataFusion]: https://github.com/apache/datafusion
