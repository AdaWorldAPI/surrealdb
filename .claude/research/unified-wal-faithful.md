# Unified WAL — the FAITHFUL answer: lance6's native transaction→manifest commit (v0.1)

> Faithfulness rule (hard): the substrate is **lance6 / lancedb**. The unified WAL is NOT
> surrealkv's WAL and NOT a bespoke WAL — it IS lance's native atomic, versioned commit.
> Lance-cited below; the lance-graph side is confirmed by the Sosumi agent (in progress).

## 1. Lance's native commit IS the WAL/ACID  (cited)
`lance-table-6.0.0/src/format/transaction.rs:4-67`, `io/commit.rs`, `io/commit/external_manifest.rs`:
- A **lance Transaction** = the unit of change — an `Operation` (Append / Update / Merge /
  Delete / Overwrite / …), serialized into `_transactions/` as protobuf.
- **1:1 transaction ↔ manifest ↔ version** (transaction.rs:26-33,60-67); each version's manifest
  references its creating transaction via `transaction_file`.
- **Durability boundary** = the atomic manifest write. Before it, nothing is visible.
- **ACID:** Atomic (1 txn = 1 manifest, all-or-nothing) · Consistent (Operation semantics) ·
  Isolation (immutable version snapshots = MVCC/time-travel) · Durable (manifest commit).
- **OCC (this is what lets TWO writers share ONE store):** commit writes a new manifest; on
  conflict, retry against the latest manifest (transaction.rs:22-30,56-64); per-Operation conflict
  rules (Append/Overwrite always succeed; Delete conflicts only with ops on the same fragments,
  transaction.rs:17-20,51-54). Concurrent committers coordinate via the commit store.
⇒ The manifest chain + `_transactions/` folder ARE the commit log. No surrealkv. No bespoke WAL.

## 2. The ONLY thing taken from the surreal side: the txn SEMANTIC (not the WAL)
`surrealkv transaction.rs:735 commit()`: a SurrealDB transaction buffers a multi-key `write_set`
and commits it as ONE `Batch` under ONE `commit_timestamp`. Faithful mapping:
  **one SurrealDB transaction == one lance Transaction (one Operation over a row-batch) == one version.**
surreal's multi-key atomicity RIDES lance's transaction atomicity. kv-lance already folds a txn's
writes into one `merge_insert` (= one lance Operation) — that part is faithful. (Answers Q1.)

## 3. Two writers, one store (Q2), faithfully
lance's native OCC already supports concurrent writers on one dataset (retry + conflict rules +
commit store). So surreal (via kv-lance) and lance-graph BOTH commit lance Transactions over the
ONE shared dataset; lance coordinates them. No second store, no shared bespoke WAL. The "unified
WAL" = lance's commit, used by both. (Answers Q2/Q3: "surrealkv wal == lancedb wal" is the wrong
axis — lance's commit IS the WAL.)

## 4. Faithfulness verdict on my kv-lance front-WAL/memtable (does it even belong?)
lance-native already gives ACID batch writes + OCC. The kv-lance WAL+memtable+flusher adds two
things lance doesn't: (a) sub-flush durability for un-committed hot writes; (b) batching many
surreal txns into fewer lance commits (version-explosion control).
- (b) is legitimate AND stays lance-faithful IF the buffer is just an un-committed Arrow batch
  flushed as ONE lance Transaction per window — NOT a separate on-disk WAL format.
- (a) is the ONLY motive for a bespoke durable WAL. Per cognitive-RISC invariant #11 ("WAL
  persists the substrate line ONLY; reconstructible from plan"), if the producer can replay the
  hot buffer from plan/AST, the hot buffer need not be independently durable.
⇒ Faithful default: **retire the bespoke on-disk kv-lance WAL**; keep only an in-memory Arrow
  batch buffer that commits one lance Transaction per window. Durability = lance's manifest commit.
  (Open: validate against the sub-µs durability requirement — see §6.)

## 5. The arc, faithfully (bottom-up)
1. **WAL** = lance6 native transaction→manifest commit. ONE store; OCC carries both writers.
2. **DAIS** = zero-copy Arrow/SoA view over that ONE lance dataset (DataFusion TableProvider);
   surreal-as-view and lance-graph read the SAME Arrow buffers — no copy, no duplication.
3. **Cognitive shader** runs over the DAIS SoA grid.

## 6. Open — Sosumi (lance-graph) confirms, then I finalize
- Does lance-graph commit via native lance Transactions (Append/merge_insert) or the lancedb
  table API? At what cadence (sub-µs batched, or per-op)? Own buffer/WAL or pure lance commit?
- That determines the shared write path: "both call lance commit" (cleanest) vs "shared in-memory
  Arrow batch buffer in front of one lance commit." Either way: ONE store, lance's commit = the WAL.

## CITATION CORRECTION (accuracy > convenience)
The OCC/conflict logic is NOT in `lance-table/format/transaction.rs` (that file, lines 1-43, is
only the protobuf wrapper). The real native model lives in the **lance crate**:
`lance-6.0.0/src/dataset/transaction.rs` — the `Operation` enum, `Transaction::conflicts_with`,
and `check_concurrent_commit` (≈L1467). lance-table's `format/transaction.rs` + `io/commit.rs`
are the serialize/manifest-write boundary. Substance of §1 stands (native 1:1 txn↔version + OCC
retry); only the file:line pointers are corrected here.

## §1 CONFIRMED in lance's own words  (lance-6.0.0/src/dataset/transaction.rs:44-66)
Quote: "retrying the commit if another writer has made a conflicting change … until it
succeeds, or fails with a 'too many retries' error. Operations prescribe the conflict
resolution rules … a transaction has a read_version … the writer checks if there have been
any other transactions committed since the read_version, and determines if the operations
conflict … If they don't conflict, the transaction can be REBASED on top of the concurrent
changes and committed." Plus: "uncommitted_changes can be used to stage changes before
committing, allowing a transaction to be split into multiple commits."
⇒ read_version + conflict-check + rebase-retry = lance-native OCC = the unified WAL for BOTH
  writers on ONE dataset. `uncommitted_changes` = lance-native staging/batching ⇒ strengthens
  §4: even the batch-buffer need is lance-native; the bespoke kv-lance on-disk WAL is retirable.
NOTE: file read returned rendering noise after L66; only L44-66 used (cross-checked vs grep:
read_version L56, Operation enum L113, RetryableCommitConflict in frag_reuse.rs:243).
