# kv-lance native-rewrite — SHARED CONTEXT (read first)

## Goal (acceptance criterion)
The SurrealDB `kv-lance` backend must read & write through **lance's native
path**, exactly as `lance-graph` does — NOT a hand-rolled LSM. Concretely:
- **write/commit** → build one Arrow `RecordBatch` from the txn's pending
  buffer and apply it with a single `lance::dataset::MergeInsertBuilder`
  (`WhenMatched::UpdateAll` / `WhenNotMatched::InsertAll`, keyed on `key`).
  One SurrealDB commit = one lance dataset version.
- **read** → `Dataset::checkout_version(v)` (or latest) → `.scan()` with a
  DataFusion filter + project, materialise, merge the pending buffer.
- **compaction/GC** → lance's own `optimize` (background_optimizer), never a
  custom flusher.

## THROW AWAY (the reinvention — orchestrator deletes these files)
`memtable.rs`, `wal.rs`, `flusher.rs`, `commit_gate.rs`, and the entire
`WritePath` apparatus. They duplicate lance 6's built-in `mem_wal`. No file
may reference them after the rewrite.

## KEEP (already native; reuse verbatim)
- `schema.rs` — Arrow KV schema + predicate builders (`build_get_predicate`,
  `build_range_predicate`, etc.).
- `tx_buffer.rs` — `PendingBuffer` (per-txn writes/deletes). Lance has no
  per-row txn buffer, so this stays.
- `cnf.rs` — `SURREAL_LANCE_*` knobs (drop the flusher-only ones).
- `background_optimizer.rs` — calls `Dataset::optimize` / `cleanup_old_versions`.
- `timeline.rs` — read-only view over `Dataset::versions()`/`checkout_version()`.
- In `mod.rs`: `build_write_batch_lance`, `build_tombstone_batch_lance`,
  `max_persisted_seq`, `DatasetHandle`, and the get/scan lance calls — these
  are the proven native calls; the rewrite REMOVES the LSM wrapper around
  them, it does not reinvent them.

## Schema (unchanged, 5 columns)
`key:Binary, val:Binary, version:UInt64, tombstone:Boolean, seq:UInt64`.
`seq` stamped per-commit from a `Datastore` `AtomicU64` seeded by
`max_persisted_seq` at open.

## Transactable contract (19 methods — full detail in
## .claude/knowledge/transactable-contract.md, AUTHORITATIVE)
lifecycle: kind/closed/writeable, commit/cancel.
reads: exists/get (pending wins; `version` ⇒ checkout_version).
writes: set/put/putc/del/delc (buffer only; CAS surfaces at lance OCC commit).
range: keys/keysr/scan/scanr (range filter + order_by + pending merge, then skip/limit).
savepoints: new_save_point/rollback_to_save_point/release_last_save_point
(pending-buffer snapshot stack).

## CONVENTIONS (mandatory for every agent)
1. **tee only.** Write your file with `tee <path> <<'RUSTEOF' … RUSTEOF`
   (overwrite). Do NOT use Write/Edit tools — they pop up for approval.
2. **Do NOT run cargo** (no build/check/test). The orchestrator runs
   `cargo clippy` ONCE at the end as the single gate.
3. **`// ///REVIEW:` sentinels.** Mark any line where you guessed a lance/
   arrow API, made an assumption, or want a tester's eyes, with a trailing
   `// ///REVIEW: <why>`. These are stripped before the PR.
4. **Read the real code before writing.** The current files in
   `surrealdb/core/src/kvs/lance/` contain the proven native calls — read
   them, reuse them, delete the LSM around them.
5. **Log** one line to `.claude/board/AGENT_LOG.md` via `tee -a` when done.
6. Match surrounding style: TABS for indentation, `crate::err::Error`,
   `#[instrument]` on public async fns, `web_time` not `std::time`.
