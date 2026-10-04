# AGENT 1 — surrealdb/core/src/kvs/lance/mod.rs (THE CORE)

Rewrite `mod.rs` to the native path. Read the CURRENT `mod.rs`,
`flusher.rs` (for `single_lance_commit`/`execute_merge`/
`build_columnar_merge_batch`), `tx_buffer.rs`, `schema.rs` first.

## Module decls (top of file)
Keep: `background_optimizer, cnf, schema, timeline, tx_buffer`.
REMOVE: `commit_gate, flusher, memtable, wal`. Remove their imports
(`Memtable`, `MemOp`, `Wal`, `WalOp`, `CommitGate`, `WritePath`, flusher types).

## Datastore struct
Fields: `dataset: Arc<RwLock<DatasetHandle>>`, `versioned: bool`,
`background_optimizer: Option<Arc<BackgroundOptimizer>>`,
`commit_seq: Arc<AtomicU64>`. DROP `write_path, commit_gate, wal, memtable`.

## Datastore::new()
Keep: open-or-create dataset, create BTREE `key` index, `max_persisted_seq`
→ seed `commit_seq`, spawn `background_optimizer`. DROP: `Wal::open`/replay,
`Memtable::new`, flusher spawn, commit_gate spawn, WritePath branching.

## Transaction struct
`done, write, versioned, pending: Arc<RwLock<PendingBuffer>>,
save_points, read_version, dataset, background_optimizer, commit_seq`.
DROP `write_path, commit_gate, wal`.

## commit()  (the heart — single native lance commit)
1. check closed/writeable. 2. `(writes, deletes) = pending.partition()`.
3. empty ⇒ mark done, return. 4. mint one `seq` from `commit_seq`.
5. build batches via `build_write_batch_lance(&writes, version, &seqs)` and
   `build_tombstone_batch_lance(&deletes, version, &seqs)` (version =
   `read_version+1` or dataset latest+1). 6. apply BOTH in ONE
   `MergeInsertBuilder::execute_reader` (move `execute_merge` from flusher
   into mod.rs). 7. mark done, `background_optimizer.notify_commit()`.
NO WAL, NO memtable, NO write_path dispatch. Delete `commit_lsm`,
`commit_legacy_gate`.

## get()/scan_impl()
Keep the existing lance read logic but DELETE the memtable branch: reads are
pending-buffer → lance (`checkout_version(version|read_version)` or latest)
→ filter/project/merge. Unversioned reads may read latest. Use `.ok()` on
`checkout_version` (clippy-clean).

## Untouched method bodies
set/put/putc/del/delc/exists/keys/keysr/scan/scanr/savepoints stay as-is
(they already operate on `pending`); only remove any `write_path`/memtable refs.
