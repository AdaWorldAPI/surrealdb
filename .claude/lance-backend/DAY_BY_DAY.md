# Day-by-Day Implementation Plan

This is the concrete checklist for the agent to fill out the
`TODO(lance-integration)` stubs in this scaffold. Each day produces
a self-contained commit that compiles and passes its own tests.

## Prep (~30 min, no code yet)

- [ ] Check out `feat/kv-lance-backend` branch from `main`
- [ ] Apply patches:
  - [ ] `PATCH-Cargo-toml.txt` → `surrealdb/core/Cargo.toml`
  - [ ] `PATCH-kvs-mod.rs` → `surrealdb/core/src/kvs/mod.rs`
  - [ ] `PATCH-kvs-config.rs` → `surrealdb/core/src/kvs/config.rs`
  - [ ] `PATCH-kvs-ds.rs` → `surrealdb/core/src/kvs/ds.rs`
- [ ] Copy `lance/` directory to `surrealdb/core/src/kvs/lance/`
- [ ] Verify `cargo check --features kv-lance` compiles (expect lots of
      `todo!()` panics at runtime — that's fine, structural compile
      should pass)
- [ ] Commit: "feat(kvs): scaffold Lance backend module (todo stubs)"

## Day 1 — Datastore opening

- [ ] Add real `lance::Dataset` field to `DatasetHandle` (mod.rs)
- [ ] Implement `Datastore::new()`:
  - [ ] `Dataset::open(path)` for existing datasets
  - [ ] `Dataset::write` with empty stream + `KvSchema::arrow_schema()`
        for new datasets
  - [ ] `create_index(&["key"], ScalarIndexType::BTree, ...)`
- [ ] Implement `Datastore::current_version()` via `dataset.version()`
- [ ] Add integration test: open empty → close → re-open works
- [ ] Commit: "feat(kvs/lance): wire Dataset open/create"

## Day 2 — Basic reads (`get` / `exists`)

- [ ] In `Transaction::get`:
  - [ ] Open snapshot via `dataset.checkout(version)`
  - [ ] Build filter via `KvSchema::build_get_predicate(&key)`
  - [ ] Run `snapshot.scan().filter(&filter).limit(Some(1)).try_into_stream()`
  - [ ] Extract `val` from the first row's `BinaryArray`
  - [ ] Return `Ok(Some(val))` or `Ok(None)`
- [ ] Test: insert via direct Lance API → `get` returns the value
- [ ] Test: `get` on missing key → returns `None`
- [ ] Test: pending-buffer read-your-writes order works
- [ ] Commit: "feat(kvs/lance): implement get() + exists()"

## Day 3 — Basic writes (`set` / `commit`)

- [ ] `Transaction::set` is already correct (writes to pending buffer)
- [ ] In `Transaction::commit`:
  - [ ] Partition pending into `writes`/`deletes`
  - [ ] Build batch via `KvSchema::build_write_batch(&writes, new_version)?`
  - [ ] Call `dataset.append(stream).await?` to flush
  - [ ] Mark `done = true`
- [ ] Test: `set` + `commit` + `get` round-trips
- [ ] Test: `cancel` discards pending writes
- [ ] Test: multiple `set`s commit atomically
- [ ] Commit: "feat(kvs/lance): implement set() + commit()"

## Day 4 — Conditional writes (`put` / `putc`)

- [ ] `put`: read-check then write (already in scaffold; verify
      against test)
- [ ] `putc`: read-check + match-on-value then write (already in
      scaffold; verify)
- [ ] Test: `put` on existing key → error
- [ ] Test: `putc` with matching `chk` → succeeds
- [ ] Test: `putc` with mismatched `chk` → error
- [ ] Test: `putc` with `None` chk on missing key → succeeds
- [ ] Commit: "feat(kvs/lance): implement put() + putc()"

## Day 5 — Deletes (`del` / `delc`)

- [ ] `del`: writes Delete entry to pending buffer (already scaffolded)
- [ ] In `commit`: handle delete entries:
  - [ ] If `LANCE_DELETE_VIA_TOMBSTONE_ROW = true`: append tombstone rows
        via `KvSchema::build_tombstone_batch`
  - [ ] Always: call `dataset.delete(KvSchema::build_delete_predicate(&deletes))`
- [ ] `delc`: read-check + match then delete (already scaffolded)
- [ ] Test: `del` + commit + `get` returns `None`
- [ ] Test: `delc` with mismatched chk → error, key still present
- [ ] Commit: "feat(kvs/lance): implement del() + delc()"

## Day 6 — Range reads (`scan` / `scanr`)

- [ ] In `Transaction::scan_impl`:
  - [ ] Build filter via `KvSchema::build_range_predicate(&start, &end)`
  - [ ] `scan().filter(...).order_by("key", direction).limit(...)`
  - [ ] Iterate the stream, materialise `Vec<(Key, Val)>`
- [ ] Merge with pending buffer:
  - [ ] For each pending `Set(k, v)` where `start <= k < end`: override
        or append
  - [ ] For each pending `Delete(k)` where `start <= k < end`: drop
        from result
- [ ] Apply skip/limit AFTER the merge (so pending and stored state
      see a consistent ordering)
- [ ] Test: `scan` of all keys returns sorted by key
- [ ] Test: `scanr` returns reverse-sorted
- [ ] Test: `scan` skip/limit work correctly
- [ ] Test: pending writes appear in scan results
- [ ] Test: pending deletes hide stored rows
- [ ] Commit: "feat(kvs/lance): implement scan() + scanr()"

## Day 7 — Range keys (`keys` / `keysr`)

- [ ] Already implemented as projection of `scan` → drop val. Verify
      tests pass.
- [ ] Optional optimisation: project only the `key` column from Lance
      to skip val deserialisation. ~3 lines of code change.
- [ ] Commit: "feat(kvs/lance): implement keys() + keysr()"

## Day 8 — Savepoints

- [ ] Already scaffolded via pending-buffer snapshots
- [ ] Test: `new_save_point` then `rollback_to_save_point` reverts
      pending changes
- [ ] Test: `new_save_point` then `release_last_save_point` keeps
      pending changes
- [ ] Test: nested savepoints (push 2, pop 1, rollback 1)
- [ ] Commit: "feat(kvs/lance): implement savepoint operations"

## Day 9 — Versioning

- [ ] Verify `get(key, Some(version))` uses `Dataset::checkout(version)`
      and not `read_version`
- [ ] Test: commit at v1, commit at v2, read at v1 sees original value
- [ ] Test: `UnsupportedVersionedQueries` returned if `versioned = false`
- [ ] Commit: "feat(kvs/lance): wire MVCC versioning to Dataset::checkout"

## Day 10 — Background optimizer

- [ ] In `BackgroundOptimizer::run_loop`, call:
  - [ ] `dataset.optimize(CompactionOptions::default())`
  - [ ] `dataset.cleanup_old_versions(retention_secs)`
- [ ] Test: spawn optimizer with 100ms interval; commit 10 batches;
      verify dataset version count converges
- [ ] Test: shutdown completes within 1s
- [ ] Commit: "feat(kvs/lance): wire background optimizer to Lance optimize"

## Day 11 — Error mapping & rough edges

- [ ] Map `lance::Error::*` to `kvs::Error::*` (write a `From` impl)
- [ ] Handle `Conflict` errors as retryable (return appropriate
      SurrealDB error variant)
- [ ] Add tracing spans around hot-path operations
- [ ] Add property test: random sequence of get/set/del/commit
      against a HashMap reference implementation, verify equivalent
      output
- [ ] Commit: "feat(kvs/lance): error mapping + property tests"

## Day 12 — Run SurrealDB test suite

- [ ] `SURREAL_TEST_KV=lance cargo test --features kv-lance`
- [ ] For each failing test:
  - [ ] Diagnose: is it a backend bug, a missing feature, or an
        intentional difference (e.g. semantics that don't apply to
        Lance)?
  - [ ] Either fix in the backend or add to a `KNOWN_DIFFERENCES.md`
- [ ] When 95%+ tests pass, write a short report with:
  - [ ] Number of tests passing
  - [ ] Known semantic differences
  - [ ] Performance characteristics (latency p50/p99 for get/set/scan)
- [ ] Commit: "test(kvs/lance): SurrealDB suite passes (95%+)"

## Phase 2 (post-POC, weeks 3-4)

- [ ] Wire `ndarray` SIMD into Lance's vector-index lookups
- [ ] Multi-bucket BindSpace sharding (1 Lance dataset per bucket)
- [ ] Expose `lance-graph`'s Cypher engine as a SurrealQL function
- [ ] Expose `blasgraph`'s GraphBLAS algebra for analytical graph
      queries

## Phase 3 (post-POC, month 2)

- [ ] PR upstream to SurrealDB
- [ ] Documentation site contribution
- [ ] Benchmarks vs RocksDB / SurrealKV / TiKV
