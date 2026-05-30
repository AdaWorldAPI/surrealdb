# AGENT 3 — surrealdb/core/src/kvs/lance/tests.rs (contract tests only)

Rewrite `tests.rs` to cover ONLY the Transactable contract against the
native backend. Read the current `tests.rs` first.

KEEP / ADAPT (remove any `write_path:` field from their `LanceConfig`
literals; they now use the single native path):
- round-trip: set+commit+get; overwrite; del+commit+get→None.
- put/putc/delc CAS (exists, match, mismatch, None/None).
- scan/scanr/keys/keysr: ordering, range bounds, skip/limit, pending merge,
  pending delete hides stored rows.
- savepoints: new/rollback/release, nested.
- versioning: commit v1, commit v2, get(key, Some(v1)) sees old;
  UnsupportedVersionedQueries when versioned=false.
- exists; cancel discards pending; closed() sticky.

DELETE ENTIRELY (these tested the reinvention that no longer exists):
- every `writepath_*`, `lsm_*`, `*recovery*`, `seq_*`, `*flush*`,
  `*commit_gate*`, `*coordinator*`, `*wal*` test.
- anything referencing `WritePath`, `Memtable`, `Wal`, `Flusher`, `CommitGate`.

Helpers (`unique_tmp_path`, etc.) stay. Leave `// ///REVIEW:` on any test
whose expectation you are unsure of under the native single-version-per-commit
model (e.g. timeline version-count assertions).
