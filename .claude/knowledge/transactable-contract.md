# Transactable Contract — what each method must guarantee

> **READ BY:** any session that implements or modifies methods on
> `kvs::lance::Transaction` (or any other `Transactable` impl).
>
> Purpose: the doc-comments on `kvs/api.rs::Transactable` are
> authoritative; this file is a one-screen condensation of the
> invariants each method must preserve, written from the point of
> view of the Lance backend. When this file and `api.rs` disagree,
> `api.rs` wins — append a correction entry to
> `.claude/board/EPIPHANIES.md`.

## The 19 methods (scaffolded in `lance/mod.rs`)

### Lifecycle (3)

| Method | Returns | Invariant |
| --- | --- | --- |
| `kind()` | `&'static str` | Stable identifier; `"lance"` for this backend. |
| `closed()` | `bool` | `true` after `commit()` or `cancel()` has returned `Ok` once. Idempotent reads. |
| `writeable()` | `bool` | Reflects the `write` flag passed at `Datastore::transaction(write, …)`. |

### Reads (2)

| Method | Lance path | Invariant |
| --- | --- | --- |
| `exists(key, version)` | `self.get(key, version).await.map(\|v\| v.is_some())` | Equivalent to `get(...).is_some()`. Must respect `version`. |
| `get(key, version)` | pending → Lance scan @ version | (1) Pending buffer wins for read-your-writes. (2) `version.is_some()` requires `versioned == true`, else `UnsupportedVersionedQueries`. (3) Tombstones in pending and tombstones in the dataset both return `None`. |

### Writes (5)

| Method | Buffer effect | Invariant |
| --- | --- | --- |
| `set(key, val)` | upsert `Set(val)` | Always succeeds; commit-time is where conflicts surface. |
| `put(key, val)` | upsert `Set(val)` | Fails with `TransactionKeyAlreadyExists` if `exists(key, None)` is true at call time. Real CAS happens at Lance OCC commit. |
| `putc(key, val, chk)` | upsert `Set(val)` if chk matches | `(Some(v), Some(w))` requires `v == w`; `(None, None)` succeeds; otherwise `TransactionConditionNotMet`. |
| `del(key)` | upsert `Delete` | Always succeeds; commit-time merges tombstone. |
| `delc(key, chk)` | upsert `Delete` if chk matches | Same match rules as `putc`. `(None, None)` is a trivial success (nothing to delete). |

All writes check `closed()` and `writeable()` first and short-circuit
with `TransactionFinished` / `TransactionReadonly` respectively.

### Range operations (4)

| Method | Direction | Implementation |
| --- | --- | --- |
| `keys(rng, limit, skip, version)` | Forward | `self.scan(...).map(\|(k, _)\| k)` (optimisation: project only `key` from Lance). |
| `keysr(...)` | Reverse | Same as `keys` but via `scanr`. |
| `scan(rng, limit, skip, version)` | Forward | Range filter + `order_by(key, ASC)` + merge with pending. |
| `scanr(...)` | Reverse | Range filter + `order_by(key, DESC)` + merge with pending. |

Crucial: merge pending overrides **before** applying skip/limit, so
that ordering is consistent across pending + stored state. The
scaffold's `scan_impl` doc-comment steps 1–6 spell this out.

### Savepoints (3)

| Method | Effect | Invariant |
| --- | --- | --- |
| `new_save_point()` | push `pending.clone()` | Snapshots include tombstones, not just writes. |
| `rollback_to_save_point()` | replace `pending` with top snapshot | Returns `NoSavePointPresent` if the stack is empty. |
| `release_last_save_point()` | pop without applying | Returns `NoSavePointPresent` if the stack is empty. Does not flush. |

The Lance backend's savepoint model is a pending-buffer snapshot
stack; Lance itself has no savepoint concept. The commit path sees
only the final (post-rollback or post-release) buffer.

### Transaction lifecycle (2)

| Method | Steps |
| --- | --- |
| `commit()` | (1) Check `closed()`, `writeable()`. (2) Partition pending. (3) Build `RecordBatch` for writes + delete predicate. (4) `Dataset::with_transaction(\|tx\| async { tx.append(batch).await?; tx.delete(predicate).await?; Ok(()) }).await`. (5) `done.store(true)`. (6) Notify the background optimizer. |
| `cancel()` | (1) Check `closed()`. (2) Clear pending buffer. (3) Clear save-point stack. (4) `done.store(true)`. Lance dataset is untouched. |

`commit()` is the only place where Lance OCC conflicts surface as
errors; map those via `kvs::Error::TransactionRetryable` (see
`lance-api-surface.md` § Error mapping).

## Cross-cutting invariants

- **`closed()` is sticky.** Once a transaction has committed or
  cancelled, every subsequent method returns
  `TransactionFinished`. Don't relax this.
- **No I/O on writes.** `set`/`put`/`putc`/`del`/`delc` mutate the
  in-memory buffer only — they never touch Lance. Lance I/O
  happens only in `commit`, `get`, `scan_impl`, and indirectly
  via the background optimizer.
- **Snapshot isolation.** `read_version` is captured at
  `Datastore::transaction()` time and held constant for the
  transaction's lifetime. Reads at `version = None` use
  `read_version`; reads at `version = Some(v)` use `v` (and
  require `versioned == true`).
- **`versioned` is set by `LanceConfig::versioned`.** When false,
  any `Some(version)` argument returns
  `UnsupportedVersionedQueries`.

## When something looks like a contract violation

1. Read the actual doc-comment on the relevant `Transactable`
   method in `surrealdb/core/src/kvs/api.rs`. That's authoritative.
2. Check whether other backends (SurrealKV in particular) make a
   different choice. If yes, that's the prior-art baseline.
3. Append a CONJECTURE to `.claude/board/EPIPHANIES.md` describing
   the apparent conflict and the proposed resolution.
4. Write a test that pins the chosen behaviour before changing
   the code.
