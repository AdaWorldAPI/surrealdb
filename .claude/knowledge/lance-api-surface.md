# Lance API Surface — what the kv-lance backend calls

> **READ BY:** any session that resolves `TODO(lance-integration)`
> comments in `surrealdb/core/src/kvs/lance/*.rs`.
>
> Purpose: one place to look up which Lance API calls the scaffold
> expects to make, so a new session doesn't have to re-grep
> `lance::Dataset` documentation to find the right method.
>
> Status: documents what the TODO comments in the scaffold call.
> If the pinned `lance` crate version (set in
> `lance-backend/patches/Cargo-toml.patch.txt`) exposes different
> signatures, **prefer the actual crate docs** (`cargo doc --open
> --package lance`) and append a correction entry to
> `.claude/board/EPIPHANIES.md`.

## Crate version

Pinned via `lance-backend/patches/Cargo-toml.patch.txt`. The surface
below targets the major version selected there. Re-verify on
upgrade.

## Dataset lifecycle

| Operation | TODO site | Expected call |
| --- | --- | --- |
| Open existing dataset | `mod.rs::Datastore::new` | `lance::Dataset::open(path).await` |
| Create empty dataset | `mod.rs::Datastore::new` | `lance::Dataset::write(stream, path, Some(WriteParams::default())).await` with an empty stream typed to `KvSchema::arrow_schema()` |
| Create scalar index | `mod.rs::Datastore::new` | `dataset.create_index(&["key"], IndexType::Scalar, Some("key_btree_idx".into()), &ScalarIndexParams::default(), replace: false).await` |
| Current version | `mod.rs::Datastore::current_version` | `dataset.version().version` (returns `u64`) |
| Snapshot at version | `mod.rs::Transaction::get`, `scan_impl` | `dataset.checkout(version).await` (returns the dataset pinned to that version) |

The `key_btree_idx` is idempotent — Lance returns `Ok` if the
index already exists, which is why `Datastore::new` can call it
unconditionally on every open.

## Write path

| Operation | TODO site | Expected call |
| --- | --- | --- |
| Append a `RecordBatch` | `Transaction::commit` | `dataset.append(stream).await` where stream yields `KvSchema::build_write_batch(&writes, version)` |
| Delete by predicate | `Transaction::commit` | `dataset.delete(KvSchema::build_delete_predicate(&deletes)).await` |
| Combined atomic txn | `Transaction::commit` | `dataset.with_transaction(\|tx\| async { tx.append(...).await?; tx.delete(...).await?; Ok(()) }).await` |

The `with_transaction` variant is what guarantees append+delete
are observable together; the standalone `append` and `delete`
calls are NOT atomic relative to each other.

## Read path

| Operation | TODO site | Expected call |
| --- | --- | --- |
| Point lookup | `Transaction::get` | `snapshot.scan().filter(KvSchema::build_get_predicate(&key))?.project(&["val", "version"])?.limit(Some(1), None)?.try_into_stream().await?` |
| Range scan | `Transaction::scan_impl` | `snapshot.scan().filter(KvSchema::build_range_predicate(&start, &end))?.order_by("key", direction)?.limit(...)?.try_into_stream().await?` |

The `limit(Some(1), None)` second arg is offset / skip (set
to `None` for point reads; pass `Some(skip as i64)` for paginated
scans).

Use `try_into_stream` (not `into_stream`) so errors propagate as
`Result` rather than panic.

## Background optimize

| Operation | TODO site | Expected call |
| --- | --- | --- |
| Compact + index refresh | `background_optimizer::run_loop` | `dataset.optimize(CompactionOptions::default()).await` |
| Prune old versions | `background_optimizer::run_loop` | `dataset.cleanup_old_versions(retention_secs).await` |

Both are idempotent and safe to call on a steady-state dataset.

## Error mapping (Day 11)

The scaffold's `kvs::Error` enum needs `From<lance::Error>`.
Expected mapping:

| `lance::Error` variant | `kvs::Error` variant | Retry? |
| --- | --- | --- |
| `DatasetNotFound` | `DsError("dataset not found")` | no |
| `CommitConflict` | `TransactionRetryable` | yes |
| `SchemaMismatch` | `DsError(...)` | no |
| `IO(_)` | `DsError("IO: …")` | no |
| anything else | `DsError(format!("{e}"))` | no |

`TransactionRetryable` lets SurrealDB's higher-level retry loop
re-run the transaction. Don't swallow `CommitConflict` silently.

## Predicate-builder cheatsheet

All predicates are built in `KvSchema` (`schema.rs`) and consumed
by the calls above. Reusing them keeps the predicate dialect
consistent and lets us unit-test predicates in isolation.

| Builder | Produces |
| --- | --- |
| `KvSchema::build_get_predicate(&key)` | `key = X'aabbcc' AND tombstone = false` |
| `KvSchema::build_range_predicate(&start, &end)` | `key >= X'..' AND key < X'..' AND tombstone = false` |
| `KvSchema::build_delete_predicate(&deletes)` | `key IN (X'..', X'..', …)` (`false` if empty) |
| `KvSchema::build_write_batch(&writes, version)` | `RecordBatch` with `tombstone = false` |
| `KvSchema::build_tombstone_batch(&deletes, version)` | `RecordBatch` with `tombstone = true` |

The hex encoding (`X'aabb'`) is what DataFusion expects for binary
literals; do not introduce another encoding.

## When in doubt

1. `cargo doc --open --package lance` — actual API for the pinned
   version.
2. The lance-graph repo (`AdaWorldAPI/lance-graph`) — uses the
   same `lance` crate. Look at how it opens datasets / runs
   scans for working reference patterns.
3. Append a CONJECTURE entry to `.claude/board/EPIPHANIES.md`,
   then write a probe (smoke test) before promoting to FINDING.
