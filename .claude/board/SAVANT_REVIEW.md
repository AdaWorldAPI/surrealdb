
---

# [SAVANT-A] kv-lance native-rewrite review — ACID + Transactable contract
**Date:** 2026-05-30  **Reviewer:** SAVANT A (read-only)  **Lens:** ACID + the 19-method Transactable contract
**Scope read:** transactable-contract.md (authoritative), 00-shared-context.md, kvs/lance/{mod.rs, tests.rs, tx_buffer.rs, schema.rs, timeline.rs}, kvs/{api.rs, err.rs}.

## Verdict summary
The rewrite is **structurally faithful** to the native single-path design: one commit folds writes + delete-tombstones into ONE `MergeInsertBuilder::execute_reader` (one Lance version), reads do pending-wins RYW then `checkout_version`/latest scan, scans merge pending before skip/limit, savepoints snapshot the whole pending buffer incl. tombstones, `closed()` is sticky, `cancel()` clears pending + savepoints. **One BLOCKER** (OCC conflict mapping) breaks the retry contract and a shipped test. Anchors adjudicated below.

---

## BLOCKER

### B1 — commit() swallows Lance OCC conflicts into `Error::Datastore`, defeating the retry contract
**File:** `surrealdb/core/src/kvs/lance/mod.rs:1068` (in `execute_merge`); surfaced from `commit()` at `mod.rs:603`.
`execute_reader(reader).await.map_err(|e| Error::Datastore(format!("lance merge_insert: {e}")))?`
maps **every** merge error — including `lance::Error::RetryableCommitConflict` / `CommitConflict` / `IncompatibleTransaction` — to an opaque `Error::Datastore(String)`.

There is already a correct `impl From<lance::Error> for Error` (`kvs/err.rs:172-202`) that maps exactly those three variants to `Error::TransactionConflict(_)`, and `Error::is_retryable()` (`err.rs:81-83`) returns true **only** for `TransactionConflict`. By stringifying the error, the commit path:
- makes `is_retryable()` return `false` for a genuine OCC conflict → SurrealDB's higher-level retry loop will NOT retry, surfacing a hard error to the user on benign contention. This is an ACID/contract violation: `transactable-contract.md` §commit() says conflicts must surface as a retryable error.
- breaks the shipped test `tests.rs:1427-1434` (`test_concurrent_same_key_yields_one_winner`), whose retry loop matches `Err(Error::TransactionConflict(_))`. Real OCC conflicts will fall through to the `Err(e) => return Err(e)` arm as `Datastore(...)`, so the task returns Err instead of retrying. The test can still pass (it only asserts `success >= 1` and tolerates failed tasks), but it does NOT actually exercise the retry path it documents — the contract guarantee is untested AND unmet.

**Fix:** propagate the typed error and let the `From` impl map it. Either:
```rust
.execute_reader(reader)
.await
.map_err(Error::from)?;     // uses impl From<lance::Error> for Error
```
or, if the error type isn't already `lance::Error` at that point, match the conflict variants explicitly before falling back to `Datastore`. Same treatment should be applied to `MergeInsertBuilder::try_new`/`try_build` only if those can return conflict variants (they cannot today — leave as Datastore). The existing `// ///REVIEW:` at `mod.rs:1068` already flags this; it is a BLOCKER, not a nit.

---

## MAJOR

### M1 — `version`-pinned reads do not respect `read_version` for `version = None`, but the doc-comment claims snapshot isolation
**File:** `mod.rs:668-675` (`get`), `mod.rs:1104-1108` (`scan_impl`); contract: `transactable-contract.md` §"Snapshot isolation".
The contract states: *"`read_version` is captured at `Datastore::transaction()` time and held constant for the transaction's lifetime. Reads at `version = None` use `read_version`."* The implementation instead reads Lance **@ latest** for `version = None` (`ds.inner.clone()`), explicitly *not* `checkout_version(read_version)`. The inline comment (`mod.rs:664-667`) defends this as intentional ("pinning to a stale `read_version` would hide rows committed by concurrent transactions").

This is a real divergence from the written contract: a long-lived read-only txn will observe writes committed by other txns after it began (read-committed, not snapshot isolation). For SurrealDB's usage (short txns, MVCC handled a layer up) this is often acceptable and may even be desired, but **mod.rs and the contract disagree**, and the contract file's own rule (§"When something looks like a contract violation") requires appending a CONJECTURE to EPIPHANIES.md and reconciling with `api.rs`. `api.rs` itself does not pin the semantics (the doc-comments on `get`/`scan` are one-liners), so this is unresolved rather than outright wrong. **Action:** either (a) honour `read_version` for `version=None` to match the stated invariant, or (b) amend `transactable-contract.md` + file an EPIPHANY recording that the Lance backend deliberately provides read-committed-at-latest for unversioned reads. Do not leave the two docs contradicting silently.

---

## MINOR

### m1 — `get()` / `scan_impl()` check `UnsupportedVersionedQueries` BEFORE `closed()`
**File:** `mod.rs:643-648` (`get`), `mod.rs:1088-1093` (`scan_impl`).
Cross-cutting invariant (`transactable-contract.md`): *"Once a transaction has committed or cancelled, every subsequent method returns `TransactionFinished`."* Here a `get(key, Some(v))` on a **closed, non-versioned** txn returns `UnsupportedVersionedQueries` instead of `TransactionFinished`, because the `!versioned && version.is_some()` guard runs first. Other backends' inherited methods (e.g. `getm` at `api.rs:203`) check `closed()` first. **Fix:** swap the order — `closed()` guard first, then the versioned-support guard. Low impact (error-precedence only) but it is a literal reading of the sticky-closed invariant.

### m2 — `seq` counter uses `Relaxed` ordering + double-increment idiom; uniqueness relies on a single atomic but documentation says "monotonic + unique across restarts"
**File:** `mod.rs:575`: `let seq = self.commit_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);`
Correctness of uniqueness holds (a single `AtomicU64` shared via `Arc`; `fetch_add` is atomic regardless of ordering), so distinct commits get distinct `seq`. Two notes: (1) `Relaxed` is fine for uniqueness but provides no happens-before w.r.t. the actual Lance commit landing; since `seq` is only a replay/debug column never read on the hot path this is acceptable — worth a one-line comment. (2) `fetch_add(1).wrapping_add(1)` means the *first* commit after a fresh open (seed = `seq_floor`) writes `seq = seq_floor + 1`. On a brand-new dataset `seq_floor = 0` so the first seq is 1 (0 is never used) — harmless but slightly surprising; the seed comment at `mod.rs:311-313` says "seeded ABOVE the maximum" which is consistent. No action required beyond a clarifying comment.

### m3 — `max_persisted_seq` does a FULL table scan on every open
**File:** `mod.rs:171-208`. Acknowledged in the doc-comment ("A future optimization can read this from manifest metadata"). Not a contract issue; flagged for the perf backlog — O(rows) startup cost per datastore open.

## NIT

### n1 — `get()` projects `version` but never reads it
**File:** `mod.rs:683` `project(&["val", "version"])` — only `val` is extracted (`mod.rs:700-709`). Projecting `version` is dead I/O (an extra column materialised per point lookup). Drop to `project(&["val"])`. (Directly relevant to Anchor 1 — see below.)

### n2 — `build_delete_predicate` / `KvSchema::build_tombstone_batch` / `build_write_batch` are now dead on the commit path
**File:** `schema.rs:78-159`. The live commit path uses `Transaction::build_*_batch_lance` (`mod.rs:922,984`) and folds deletes as tombstone rows, so `build_delete_predicate` (predicate-style delete) and the `KvSchema` batch builders are exercised only by `schema.rs` unit tests. Harmless (kept `#[allow(dead_code)]`), but a future reader may mistake `build_delete_predicate` for a live code path. Consider a `// not used by the native commit path` note.

---

## ANCHOR ADJUDICATIONS

### Anchor A — mod.rs:507 & mod.rs:581: per-row `version` stamp = `read_version + 1` vs `dataset-latest + 1`
**VERDICT: Keep `read_version + 1` (the code as written). The choice is functionally moot for reads but `read_version + 1` is the only snapshot-stable option; do NOT switch to dataset-latest+1.**

Definitive reasoning:
1. **The `version` column is never read by any query path.** Confirmed by grep: no predicate references it (`build_get_predicate`/`build_range_predicate` filter only `key` + `tombstone`); `get` projects it (`mod.rs:683`) but extracts only `val`; the sole consumers are the test helper `scan_versions_for_tests` (`mod.rs:385`) and the unit assertion. **`get(key, Some(v))` resolves entirely via `Dataset::checkout_version(v)`** — `v` is a Lance *dataset* version, not the per-row column. Therefore both candidates produce identical observable read behaviour, and timeline correctness (which also keys off Lance dataset versions, `timeline.rs:122-133`) is independent of this column.
2. Given it only affects the column's *self-consistency*: `read_version + 1` is captured at txn-open and is immune to concurrent commits landing between open and commit. `dataset-latest + 1`, evaluated at commit time, would drift upward under contention and could even collide semantics across two racing commits that both read the same "latest" before either lands. So `read_version + 1` is the principled pick.
3. **Caveat to document (not a blocker):** under OCC contention the *actual* resulting Lance dataset version may be `> read_version + 1` (Lance rebases the commit onto concurrent versions). So the schema doc-comment's claim that `version` == "the Lance dataset version at write time" (`schema.rs:16-18`) is aspirational, not guaranteed. Since nothing reads the column this is cosmetic; recommend softening the comment to "the txn's read-snapshot version + 1 (an MVCC convenience stamp; not guaranteed equal to the resulting Lance dataset version under contention)".
4. Bonus: drop the dead `version` projection in `get` (nit n1).

**Recommendation:** resolve the `// ///REVIEW:` at `mod.rs:507` and `mod.rs:581` by deleting the sentinel and keeping `read_version.saturating_add(1)`, with the schema doc-comment softened per (3).

### Anchor B — tests.rs:1029: `get_at_specific_version` semantics under MergeInsert delete-then-insert (deletion vectors)
**VERDICT: The test's expectation is SOUND and correctly scoped. Keep it as-is.**

The test (`tests.rs:1034-1037`) only asserts the **safety** property: a version-pinned read `get(k, Some(v_after_first))` MUST NOT observe the future `v2` write (`assert_ne!(at_v1, Some("v2"))`), and the `// ///REVIEW:` comment explicitly states it tolerates BOTH `Some("v1")` and `None` for the pinned read. This is the right call because:
1. `checkout_version(v_after_first)` pins the Lance dataset to the manifest as it stood after commit #1. The merge that produced commit #2 cannot retroactively alter an earlier version's manifest in Lance's MVCC model, so observing `v2` at `v_after_first` is impossible — the assertion can never spuriously fail and correctly pins the time-travel safety guarantee.
2. Whether the pinned read yields `Some("v1")` vs `None` depends on Lance internals (does the v1 manifest see the row as live, vs. a deletion-vector/upsert representation that hid it). The contract (`transactable-contract.md` §reads) requires versioned reads respect `version` and hide tombstones — it does NOT mandate that an overwrite at v2 leaves v1 readable; that's a stronger time-travel guarantee SurrealDB does not promise. So tolerating both is contract-correct, not a cop-out.
3. The companion assertions are also sound: read @ `v_initial` (pre-any-write) MUST be `None` (`tests.rs:1043`) — correct, the key did not exist in that manifest; read @ latest MUST be `v2` (`tests.rs:1025`) — correct.

**One observation (not a defect in the test, but a coverage gap):** because the MergeInsert is keyed on `key` with `UpdateAll`, commit #2 *replaces* the single row for `k` rather than appending a second row. Whether `checkout_version(v_after_first)` then sees `v1` is exactly the behaviour the POC is unsure about — and since the test tolerates both, **the stronger time-travel property "old versions remain readable" is left unpinned.** If SurrealDB ever needs `get(key, Some(old_v)) == Some(old_value)` (true historical point-read), a follow-up test must pin `Some("v1")` and the commit path may need to stop relying on in-place upsert for historical fidelity. Recommend filing this as an EPIPHANY/backlog item, but the current test is correct for the current (weaker) contract. Resolve the `// ///REVIEW:` by keeping the test and noting the deferred stronger-guarantee question.

---

## Notes on items NOT flagged (verified correct)
- **One-version-per-commit folding:** writes + tombstones share one Arrow schema and stream through a single `execute_reader` (`mod.rs:588-603`), so a write+delete commit lands as ONE Lance version. `tests.rs:1587-1648` pins this (`versions_after == versions_before + 1`). Correct per shared-context goal.
- **Pending partition / pending-wins RYW / tombstone→None:** `get` checks pending first (`mod.rs:652-657`), Delete→None; correct. `commit` partitions via `PendingBuffer::partition` (`tx_buffer.rs:78-88`); empty-buffer commit still flips `done` (`mod.rs:568-571`) — correct (a no-op commit must still close the txn).
- **Scan merge order:** pending overlaid into a `BTreeMap` BEFORE skip/limit (`mod.rs:1184-1261`), range half-open via `>= start && < end` applied to BOTH Lance predicate (`schema.rs:169-175`) and the pending overlay (`mod.rs:1194-1195`); Backward reverses the BTreeMap. Correct and matches contract §Range.
- **Savepoints:** snapshot is a full `PendingBuffer::clone` incl. tombstones (`mod.rs:875`); `tests.rs:894-914` pins tombstone restoration; rollback/release pop with `NoSavePointPresent` on empty (`mod.rs:891,907`). Correct.
- **closed() sticky + cancel clears pending+savepoints:** `mod.rs:617-625`. Correct.
- **`build_*_batch_lance` length guards:** `mod.rs:936,993` reject seq/row mismatch in all builds. Good defensive check.

**Overall:** ship-blocking item is **B1** (OCC conflict mapping). M1 needs a doc/behaviour reconciliation. Both anchor sentinels can be resolved as: keep `read_version+1`; keep the version-test as-is. Remaining items are minor/nit polish.

---

# [SAVANT-B] Lance 6.0.0 / lancedb 0.29 API-Correctness Review — 2026-05-30

Reviewer: SAVANT B (read-only). Lens: every `lance` / `lance-index` / `arrow`
call in the rewritten native read/write path, verified against the pinned
source at `/root/.cargo/registry/.../lance-6.0.0`, `lance-index-6.0.0`,
`lance-core-6.0.0`, `arrow-array-58.3.0`. Files reviewed: `kvs/lance/mod.rs`,
`background_optimizer.rs`, `timeline.rs`, `schema.rs` (+ `cnf.rs`, `err.rs`,
`config.rs`, `tests.rs` for adjudication).

## API-signature verification (all PASS unless noted)

| Call site | Pinned-source signature | Verdict |
| --- | --- | --- |
| `MergeInsertBuilder::try_new(Arc<Dataset>, Vec<String>) -> Result<Self>` | merge_insert.rs:412 | PASS — mod.rs:1060 `try_new(arc_ds, vec!["key".into()])` |
| `.when_matched(WhenMatched) -> &mut Self` ; `WhenMatched::UpdateAll` | merge_insert.rs:467 / enum:260,264 | PASS |
| `.when_not_matched(WhenNotMatched) -> &mut Self` ; `WhenNotMatched::InsertAll` | merge_insert.rs:475 / enum:294,298 | PASS |
| `.try_build() -> Result<MergeInsertJob>` | merge_insert.rs:561 | PASS |
| `MergeInsertJob::execute_reader(self, impl StreamingWriteSource) -> Result<(Arc<Dataset>, MergeStats)>` | merge_insert.rs:583 | PASS — returns `Arc<Dataset>` (NOT `Dataset`). mod.rs:1060-1070 binds `(new_ds, _stats)` then `Arc::try_unwrap(new_ds).unwrap_or_else(|arc| (*arc).clone())`. Correct for the `Arc<Dataset>` return. |
| `impl<I> StreamingWriteSource for RecordBatchIterator<I>` | lance-datafusion-6.0.0/utils.rs:86 | PASS — the `RecordBatchIterator` passed to `execute_reader` satisfies the bound; `Send` met. |
| `Dataset::open(uri: &str) -> Result<Self>` | dataset.rs:422 | PASS — mod.rs:219 |
| `Dataset::write(impl RecordBatchReader+Send+'static, impl Into<WriteDestination>, Option<WriteParams>) -> Result<Self>` | dataset.rs:748 ; `From<&str> for WriteDestination` write.rs:91 | PASS — mod.rs:246 `write(empty_reader, path, Some(WriteParams::default()))`, `path:&str` → WriteDestination OK |
| `Dataset::checkout_version(impl Into<refs::Ref>) -> Result<Self>` ; `From<u64> for Ref` | dataset.rs:427 ; refs.rs:40 | PASS — mod.rs:670/1106, timeline.rs:126 pass `u64` |
| `Dataset::version(&self) -> Version` ; `Version{version:u64, timestamp:DateTime<Utc>, metadata}` | dataset.rs:1980 / 202-211 | PASS — `.version().version` (mod.rs:351, timeline.rs:89) and `v.timestamp.timestamp_micros()` (timeline.rs:112) both valid |
| `Dataset::versions(&self) -> Result<Vec<Version>>` | dataset.rs:2000 | PASS — timeline.rs:102 |
| `Dataset::scan(&self) -> Scanner` | dataset.rs:1359 | PASS |
| `Scanner::filter(&str) -> Result<&mut Self>` | scanner.rs:1206 | PASS |
| `Scanner::project<T:AsRef<str>>(&[T]) -> Result<&mut Self>` | scanner.rs:1128 | PASS — `&["val","version"]`, `&["key","val"]`, `&["seq"]` etc. all OK |
| `Scanner::limit(Option<i64>, Option<i64>) -> Result<&mut Self>` | scanner.rs:1407 | PASS — `.limit(Some(1), None)` |
| `Scanner::order_by(Option<Vec<ColumnOrdering>>) -> Result<&mut Self>` ; `ColumnOrdering::asc_nulls_first(String)`/`desc_nulls_first(String)` | scanner.rs:1662 / 198,214 | PASS — mod.rs:1123-1130 |
| `Scanner::try_into_stream(&self) -> BoxFuture<Result<DatasetRecordBatchStream>>` | scanner.rs:1950 | PASS (awaited) |
| `DatasetIndexExt::create_index(&mut self, &[&str], IndexType, Option<String>, &dyn IndexParams, bool) -> Result<IndexMetadata>` | index.rs:814 | PASS — mod.rs:260 `create_index(&["key"], IndexType::BTree, Some("key_btree_idx".into()), &index_params, false)` |
| `IndexType::BTree` | lance-index/lib.rs:108 | PASS |
| `ScalarIndexParams::for_builtin(BuiltinIndexType) -> Self` ; `impl IndexParams for ScalarIndexParams` ; `BuiltinIndexType::BTree` | scalar.rs:128 / 150 / enum:60 | PASS — mod.rs:259 |
| `optimize::compact_files(&mut Dataset, CompactionOptions, Option<Arc<dyn IndexRemapperOptions>>) -> Result<CompactionMetrics>` ; `CompactionMetrics{fragments_removed,fragments_added}` | optimize.rs:741 / 544-548 | PASS — bg_optimizer.rs:145 `compact_files(&mut ds.inner, CompactionOptions::default(), None)`; reads `.fragments_removed/.fragments_added` |
| `cleanup::cleanup_old_versions(&Dataset, CleanupPolicy) -> Result<RemovalStats>` ; `RemovalStats{bytes_removed:u64, old_versions:u64,...}` ; `CleanupPolicy{before_timestamp:Option<DateTime<Utc>>, error_if_tagged_old_versions:bool, ...}` all-pub + `Default` | cleanup.rs:1018 / 82-89 / 889-907,922 | PASS — bg_optimizer.rs:191-208. Struct-literal `{ before_timestamp: Some(cutoff), error_if_tagged_old_versions:false, ..Default::default() }` compiles (all fields pub, Default exists). Reads `stats.bytes_removed/.old_versions`. |
| `RecordBatch::try_new(SchemaRef, Vec<ArrayRef>)` ; `RecordBatchIterator::new(I, SchemaRef)` | arrow-array-58.3.0/record_batch.rs:263 / 930 | PASS — schema.rs + mod.rs build paths |

### Arrow version alignment — RESOLVED, no skew
surrealdb/core/Cargo.toml pins `arrow-array = "58"`, `arrow-schema = "58"`
(lines 113-114) and `lance = "=6.0.0"`. lance-6.0.0 itself depends on
arrow-array/arrow-schema `58.0.0` (lance Cargo.toml:185-205). Both resolve to
arrow 58.3.0, so the top-level `arrow_array::*`/`arrow_schema::*` types
SurrealDB constructs unify with lance's internal arrow types. The merge/scan
calls type-check. (The earlier lance-1.0.4-era `lance::deps::arrow_*`
indirection is genuinely unnecessary now.)

---

## ADJUDICATION 1 — OCC commit conflict mapping (mod.rs:1068)

BLOCKER (contract violation).

Anchor: `execute_merge` maps EVERY merge error with
`.map_err(|e| Error::Datastore(format!("lance merge_insert: {e}")))?`
(mod.rs:1068). The `// ///REVIEW` asks whether lance OCC conflict should map
to a retryable error. Per `transactable-contract.md` (`commit()` row +
"Cross-cutting"): "commit() is the only place where Lance OCC conflicts
surface as errors; map those via kvs::Error::TransactionRetryable."

Findings against pinned source:
1. The retryable kvs variant is `Error::TransactionConflict(String)`
   (kvs/err.rs:39-41), and `Error::is_retryable()` matches exactly that
   variant (err.rs:81-83). There is NO `Error::TransactionRetryable` variant —
   the contract doc + the REVIEW comment both use a name that does not exist;
   the real target is `TransactionConflict`. (Doc-rot; cite err.rs.)
2. A CORRECT `impl From<lance::Error> for Error` ALREADY EXISTS
   (err.rs:171-203) and maps the conflict variants properly:
   - `lance::Error::RetryableCommitConflict {..}` → `TransactionConflict`
   - `lance::Error::CommitConflict {..}` → `TransactionConflict`
   - `lance::Error::IncompatibleTransaction {..}` → `TransactionConflict`
   All three variants exist in lance-core-6.0.0/error.rs (97 / 104 / 110), and
   `lance::Error` is the re-export of `lance_core::Error` (lance/lib.rs:75).
3. THE BUG: `execute_merge` bypasses that `From` impl. By string-formatting
   into `Error::Datastore`, a genuine OCC conflict is flattened to a
   non-retryable `Datastore` error. `is_retryable()` returns false, so
   SurrealDB's higher-level retry loop will NOT retry — exactly the contract
   breach the REVIEW flags. `put`/`putc`/`delc` CAS-on-commit semantics
   (which the doc-comments at mod.rs:738-740 promise resolve via "Lance OCC …
   conflict-error and must retry") are therefore silently broken.
   FIX: `.map_err(Error::from)` (or `?` on a `lance::Error`) so the existing
   conversion runs. Do NOT hand-roll a new mapping; the From impl is correct.

4. ADDITIONAL GAP (same fix is insufficient on its own): `MergeInsertJob`
   has BUILT-IN retry. `try_new` defaults `conflict_retries = 10`,
   `retry_timeout = 30s` (merge_insert.rs:455-456), and `execute()` →
   `execute_with_retry` (merge_insert.rs:1343) internally catches
   `Error::RetryableCommitConflict` and retries with `checkout_latest`
   (retry.rs:97-120). Consequences:
   - In normal contention, `RetryableCommitConflict` is consumed INSIDE lance
     and the caller never sees it — lance silently re-runs the merge.
   - When the 10 retries are EXHAUSTED, `execute_with_retry` returns
     `Error::too_much_write_contention(...)` =
     `lance::Error::TooMuchWriteContention` (retry.rs:126-129; variant at
     lance-core error.rs:117, Display "Too many concurrent writers. {message}").
   - The existing `From` impl does NOT have an arm for `TooMuchWriteContention`;
     it falls through the catch-all `other => Error::Datastore(...)`
     (err.rs:200). So even after fixing #3, an exhausted-contention failure is
     STILL non-retryable.
   RECOMMENDATION: (a) fix #3 (`Error::from`), AND (b) add a
   `lance::Error::TooMuchWriteContention { .. } => Error::TransactionConflict(..)`
   arm to the `From` impl in err.rs so the terminal-contention case is also
   retryable. (b) requires editing err.rs, which is outside agent-1's
   "mod.rs only" scope — flag to the orchestrator.

Citations: lance-core-6.0.0/src/error.rs:97,104,110,117;
lance-6.0.0/src/dataset/write/retry.rs:75-130;
lance-6.0.0/src/dataset/write/merge_insert.rs:412(try_new defaults),1326-1343(execute→retry);
surrealdb/core/src/kvs/err.rs:39-43,79-84,171-203.

---

## ADJUDICATION 2 — "one MergeInsert == exactly one new dataset version"
(tests.rs:1503, 1549, 1614; the regression test 1587-1648)

PASS — the invariant is API-correct; the assertions are sound.

Does ONE `execute_reader` (writes + tombstones folded) yield EXACTLY one new
version? Traced through pinned source:
- `execute_reader` → `execute` → builds ONE `Transaction::new(version,
  Operation::Update{..}, None)` (merge_insert.rs:1738; the merge folds
  insert+update+delete into a SINGLE `Operation::Update`, lines 1644/1712).
- Commit is ONE `CommitBuilder::execute(transaction) -> Result<Dataset>`
  (merge_insert.rs:1948; commit.rs:183) → ONE new manifest = ONE new version.
- mod.rs streams the write-batch AND the tombstone-batch through the SAME
  `RecordBatchIterator` into that single merge, so a mixed write+delete commit
  lands as exactly ONE version. The regression test's strict
  `versions_after == versions_before + 1` (tests.rs:1620) is CORRECT, and
  there is no torn write-before-delete intermediate (no separate
  `Dataset::delete` op). Verdict on tests.rs:1614 = CONFIRMED, may TIGHTEN.
- `test_timeline_view_reads_historical_state` (1549): one commit advances the
  version by exactly 1, so `v_after > v_before` holds. CONFIRMED.

Does background optimize add versions during these tests? NO, under default
config:
- Tests use `LanceConfig::default()` (only carries `versioned`) and set NO
  `SURREAL_LANCE_*` env var, so `LANCE_BACKGROUND_OPTIMIZE_ENABLED=true`
  (cnf.rs:15) → the optimizer task IS spawned.
- BUT its loop never wakes within a millisecond-scale test:
  * time branch: first `sleep(LANCE_OPTIMIZE_INTERVAL_NS)` = 300s
    (cnf.rs:30-33) — never elapses in-test.
  * write-count branch: `notify_commit` only calls `notify_one()` once
    `write_count >= LANCE_OPTIMIZE_AFTER_N_WRITES` = 1000 (cnf.rs:23;
    bg_optimizer.rs:94-96). These tests commit 1-2 times. Never fires.
  ⇒ `compact_files`/`cleanup_old_versions` never run during the tests; no
  version is added or removed.
- Independently, lance's in-commit `auto_cleanup_hook` is a no-op here: it
  returns `None` unless the dataset manifest config carries
  `lance.auto_cleanup.interval` (cleanup.rs build_cleanup_policy:returns
  Ok(None) when the key is absent), which SurrealDB never sets. And cleanup
  only REMOVES old manifests — it never adds a version.

Therefore:
- tests.rs:1509-1514 `versions.len() >= v_start + 2`: SOUND. With the optimizer
  quiescent it is in fact EXACTLY `v_start + 2`; the `>=` lower bound is a safe
  (if loose) choice. Could TIGHTEN to `== v_start + 2` given the above, but the
  loose bound is defensible for robustness if env defaults ever change.
- tests.rs:1620 `== versions_before + 1`: SOUND and exact. The comment's worry
  ("becomes +2 if inserts and deletes apply as two separate lance operations")
  does NOT occur — lance folds them into one `Operation::Update`.

Note (not blocking): the baseline `v_start`/`versions_before` is captured AFTER
`Datastore::new`, which itself produces 2 versions (empty `write` = v1, then
`create_index` commit = v2, since `LANCE_CREATE_KEY_INDEX_ON_OPEN=true`). Since
the tests snapshot the baseline post-open, this does not affect the deltas.

---

## OTHER FINDINGS

MINOR (doc-rot, mod.rs:228-229, 920-921, and the `read_version` REVIEW at
507/581): comments claim "arrow-array/schema = 57", "lance 4.0", "lance-1.0.4".
Actual pins are arrow 58 / lance 6.0.0. No correctness impact (the code uses
top-level `arrow_*` which is correct for the 58/58 alignment), but the
provenance comments are stale and misleading for the next reader. Recommend
updating the version numbers in the comments.

NIT (mod.rs:273-277): `create_index` re-open idempotency relies on matching
`e.to_string().contains("already exists")`. lance returns an index-exists error
when `replace=false`; the substring is not a stable API contract across lance
versions. Low risk for 6.0.0 but brittle. The same fragile-string pattern at
err.rs:273 is acceptable for now; flag for a future hardening pass.

OBSERVATION (not in scope but load-bearing for compile): mod.rs:1267-1273 notes
tests.rs / integration_tests still reference removed items (WritePath, old
LanceConfig fields, commit_gate, memtable). I confirmed tests.rs uses only
`LanceConfig::default()` + `Datastore`/`Transaction` APIs that exist, and the
config.rs REVIEW (no `retention_ns` field) is consistent. The lance-native
test surface I adjudicated (timeline + version-count tests) is self-consistent
with the rewritten mod.rs.

## VERDICT SUMMARY
- mod.rs:1068 OCC mapping → BLOCKER (bypasses the correct `From<lance::Error>`;
  conflicts become non-retryable `Datastore` errors; plus `TooMuchWriteContention`
  unmapped). Fix: `.map_err(Error::from)` + add a `TooMuchWriteContention` arm in err.rs.
- tests.rs:1503/1549/1614 version-count → PASS (one merge = one version,
  confirmed against `Operation::Update` + single `CommitBuilder::execute`;
  optimizer proven quiescent under default config).
- All MergeInsert / Dataset / Scanner / create_index / optimize / cleanup /
  arrow signatures → PASS against pinned 6.0.0 source.
- Arrow version skew → NONE (58 == 58).
- Stale "57 / lance 4.0 / 1.0.4" comments → MINOR doc-rot.
- create_index "already exists" string match → NIT (brittle).

---

# [SAVANT-C] 2026-05-30 — Rust-idiom / clippy-readiness / test-coverage pre-screen

LENS: pre-screen the ONE budgeted `cargo clippy` so it passes first try; Rust
idiom; test coverage of the Transactable contract. READ-ONLY review.
Files read: lance/{mod,tests,schema,cnf,tx_buffer,background_optimizer,timeline}.rs,
kvs/{mod,config,err,ds}.rs, mac/mod.rs, lib.rs, Makefile.ci.toml, root Cargo.toml
[workspace.lints], .clippy.toml, core/Cargo.toml [lints], and the pinned
lance-6.0.0 / lance-index-6.0.0 source in the cargo registry.

## ★ #1 GATE-DEFINING FINDING — which clippy invocation? (BLOCKER for the gate's VALIDITY, not for the code)

The stock `cargo make ci-clippy` will NOT lint a single line of the lance
rewrite, so a green run would be a FALSE PASS:
- `Makefile.ci.toml:85` ci-clippy = `cargo clippy --workspace --all-targets
  --features ${ALL_FEATURES} --tests --benches --bins -- -D warnings`.
- `Makefile.toml:13` ALL_FEATURES =
  `allocator,allocation-tracking,storage-mem,storage-surrealkv,storage-rocksdb,storage-tikv,scripting,http,jwks,ml,surrealism,cli`
  → contains NO kv-lance / storage-lance.
- The SDK crate (`surrealdb/Cargo.toml`) defines NO `storage-lance` passthrough;
  `kv-lance` exists ONLY in `core/Cargo.toml:27`. So ALL_FEATURES cannot pull it
  in transitively.
- Every lance file begins `#![cfg(feature = "kv-lance")]` and `kvs/mod.rs:36-37`
  gates `mod lance;` on it. With the feature off, the whole module + tests.rs are
  cfg-stripped before clippy sees them.

ACTION FOR ORCHESTRATOR (pick one, do NOT run plain `cargo make ci-clippy`):
  `cargo clippy -p surrealdb-core --features kv-lance --tests -- -D warnings`
(optionally add the other storage features). Everything below assumes THIS
invocation. NB: `-D warnings` promotes every `[workspace.lints.clippy] = "warn"`
to a hard error, and `core/Cargo.toml:261 workspace = true` means core inherits
that table.

## COMPILE PRE-SCREEN — all GREEN (verified against pinned 6.0.0 source)

- Deleted-module refs (memtable/wal/flusher/commit_gate/WritePath/MemOp): NONE
  live anywhere in `kvs/`. Only matches are (a) prose in doc-comments
  (mod.rs:30/43/463/1037/1268, tests.rs:10-11/1092, config.rs:297-299) and (b)
  the *other* backends' own `background_flusher.rs` (rocksdb/surrealkv) which are
  unrelated. The `rocksdb/cnf.rs` "memtable" hits are RocksDB knobs, not lance.
- config.rs `write_path` token: doc-comment ONLY (config.rs:297-299, describing
  what was removed). `LanceConfig` struct (config.rs:302) carries exactly one live
  field `versioned: bool`; `from_params(_params)` is correctly underscore-prefixed.
  NOT live → no dead-code/unused warning.
- execute_merge imports + Arc::try_unwrap: CORRECT. `MergeInsertBuilder::try_new(
  Arc<Dataset>, Vec<String>)`, `try_build`, `execute_reader(impl
  StreamingWriteSource) -> Result<(Arc<Dataset>, MergeStats)>` all match. Since
  `execute_reader` yields `Arc<Dataset>`, mod.rs:1070 `Arc::try_unwrap(new_ds)
  .unwrap_or_else(|arc| (*arc).clone())` is the right idiom (uses
  `unwrap_or_else`, NOT `.unwrap()` → no `unwrap_used` hit). Imports
  `lance::dataset::{MergeInsertBuilder,WhenMatched,WhenNotMatched}` resolve
  (re-exported at lance dataset.rs:128-131).
- All other lance API call-sites verified against source: `Dataset::{open,write,
  checkout_version(impl Into<refs::Ref>; From<u64> exists),version()->Version{
  version:u64,timestamp:DateTime<Utc>},versions()->Result<Vec<Version>>}`;
  `WriteDestination: From<&str>`; `create_index(&[&str],IndexType,Option<String>,
  &dyn IndexParams,bool)` (ScalarIndexParams coerces to &dyn IndexParams);
  `ScalarIndexParams::for_builtin(BuiltinIndexType::BTree)`; Scanner
  `filter/project/limit(Option<i64>,Option<i64>)/order_by(Option<Vec<ColumnOrdering>>)
  /try_into_stream`; `ColumnOrdering::{asc,desc}_nulls_first(String)`;
  `optimize::compact_files(&mut Dataset,CompactionOptions,None)` →
  CompactionMetrics{fragments_removed,fragments_added}; `cleanup::{CleanupPolicy
  (impl Default EXISTS @cleanup.rs:922; pub before_timestamp/
  error_if_tagged_old_versions),cleanup_old_versions(&Dataset,CleanupPolicy)->
  RemovalStats{bytes_removed,old_versions}}`. All field/arg shapes match.
- Arrow version skew: NONE. Cargo.lock has a SINGLE arrow-array = 58.3.0; lance
  6.0.0 depends on the same → direct `arrow_array::*` types == lance's internal
  arrow types. (Note: the in-code comments say "arrow 57 / lance 4.0 / lance-1.0.4"
  — see MINOR doc-rot below; harmless to the compiler.)
- Macros: `lazy_env_parse!` is `#[macro_export]` (mac/mod.rs:15) incl. the
  `duration` arm (mac/mod.rs:64) → cnf.rs needs no `use`, matches surrealkv/cnf.rs.
  `info!`/`#[instrument]`/`debug!`/`warn!`/`error!` available crate-wide via
  `#[macro_use] extern crate tracing;` (lib.rs:27). background_optimizer uses
  fully-qualified `tracing::*` — also fine.
- `Error::TransactionConflict(String)` (err.rs:41) exists; `From<lance::Error>`
  (err.rs:171) exists; tests.rs:1429/1436 reference it correctly.
- Import audit (mod.rs/schema/timeline/tx_buffer): no unused imports. Trait
  imports that show count==1 are USED for method resolution, not dead:
  `DatasetIndexExt` (→ `.create_index`), `Sprintable` (→ `.sprint()` inside the
  `#[instrument(fields(key=key.sprint()))]` expansions), `futures::TryStreamExt`
  (→ `.try_next()`). All `#[allow(dead_code)]`/`#[allow(unused_imports)]` sites
  (DatasetHandle.path, PendingBuffer::{len,is_empty}, KvSchema impl block,
  cnf LANCE_DELETE_VIA_TOMBSTONE_ROW/COMMIT_MAX_BATCH_ROWS, BackgroundOptimizer
  fields, Datastore::shutdown, timeline re-export) are legitimately needed.

## CLIPPY PRE-SCREEN under `--features kv-lance -- -D warnings` — essentially CLEAN

Walked the workspace lint table (Cargo.toml:352-463) item-by-item against the
non-test code. No hits found for: unwrap_used (the only bare `.unwrap()` calls are
schema.rs:203-217 inside `#[cfg(test)]`, exempt via `.clippy.toml
allow-unwrap-in-tests=true`; production uses `unwrap_or_else`), redundant_clone /
implicit_clone / unnecessary_to_owned (every `.clone()` is an Arc clone, an
owned-key clone before a move-across-await, or a snapshot clone; `seqs.to_vec()`
and `"key".to_string()` are required by the `UInt64Array::from(Vec)` /
`ColumnOrdering(String)` signatures), used_underscore_binding /
no_effect_underscore_binding (`_lock`/`_v`/`_stats` RHS all have side effects or
are plain `_`), assigning_clones, get_unwrap, fallible_impl_from (the
`From<lance::Error>` is non-panicking), explicit_into_iter_loop. Default-warn
lints also clear: needless_return (all `return`s are early-guard exits; tails are
bare exprs), single_match (the create_index match has 3 arms, not 1+`_`),
collapsible_if (mod.rs:1194 is one combined condition), manual_ok_err
(the `match …ok(){Some(s)=>s,None=>return}` is Option-flow, not Result→Option).
`unused_async` is `allow` (Cargo.toml:455) so the no-await async helpers are safe.
`allow_attributes` is `allow` (Cargo.toml:360) so the `#[allow(...)]` attrs do NOT
need converting to `#[expect]`.

## PRE-CLIPPY FIX LIST (file:line → change)  [all MINOR/NIT — none block the gate IF lance is excluded; none block clippy IF the kv-lance run is used]

1. [BLOCKER — process, not code] Orchestrator MUST run clippy WITH `--features
   kv-lance` (see ★#1). Otherwise the gate validates nothing. No file edit.

2. [MINOR] Stale version comments (doc-rot; cosmetic, compiler-harmless):
   - mod.rs:228-229 "lance 4.0 and our Cargo.toml both pin arrow-array/schema =
     "57"" → actual pins are lance =6.0.0 and arrow =58.
   - mod.rs:920-921 "the lance-1.0.4 era".
   - background_optimizer.rs:138-143 & 174-178 "lance 4.0".
   - timeline.rs:16 "Lance 6.0.0" (this one is correct).
   Fix: s/57/58/, s/lance 4.0|1.0.4/lance 6.0.0/ in those comments.

3. [MINOR] mod.rs:505-507 — the `read_version` doc-comment claims "`dead_code` is
   therefore allowed on the read side", but the field IS read at mod.rs:581
   (`self.read_version.saturating_add(1)`) and carries NO `#[allow(dead_code)]`
   (correctly, since it's used). Delete the misleading "dead_code is allowed"
   clause to avoid implying a dormant allow.

4. [NIT] mod.rs:273 — `Err(e) if e.to_string().contains("already exists")` is a
   brittle string-match for index idempotency (already flagged by a prior SAVANT).
   Not a clippy issue; cosmetic robustness only.

## ADJUDICATION of `// ///REVIEW:` anchors (clippy/idiom angle only)

- mod.rs:507 & 581 (read_version+1 vs dataset-latest+1): from a clippy/idiom
  standpoint NO problem — field is used, `saturating_add` is fine, no unused field
  introduced by the version-stamp choice. (Semantic correctness of the stamp is
  for a behavioural reviewer; idiom-wise clean.)
- mod.rs:1068 (map OCC conflict to retryable instead of opaque Datastore): idiom
  note — `.map_err(Error::from)` would reuse the existing `From<lance::Error>`
  and is MORE idiomatic than the inline `format!`. A prior SAVANT already filed
  this as a BLOCKER on correctness grounds; I concur it is also the more idiomatic
  Rust. Not a clippy lint, so it will NOT fail the gate, but worth the one-line
  change.
- mod.rs:1267-1271 (comment: "tests.rs + integration_tests still reference removed
  items … will NOT compile"): STALE/INACCURATE. I read all 1648 lines of tests.rs
  — it references ONLY live items (`LanceConfig::default()`, `LanceConfig{versioned:
  false}` @1056/1074, the test-only accessors, and standard Transactable methods).
  There are ZERO references to WritePath / write_path / commit_gate / memtable /
  flusher_tick. tests.rs compiles against the rewritten surface. Recommend deleting
  this misleading block (cosmetic; harmless if left).
- tests.rs:1029/1503/1549/1614 (version-count assumptions): idiom-wise fine
  (lower-bound `>=` asserts are robust); semantic adjudication already covered by a
  prior SAVANT (one-merge = one-version → PASS).

## TEST COVERAGE of the Transactable contract — STRONG, two gaps

Per-method coverage in tests.rs (19 methods): kind✓ closed✓ writeable✓ commit✓
cancel✓ exists✓ get✓(+versioned) set✓ put✓ putc✓ del✓ delc✓ keys✓ keysr✓ scan✓
scanr✓ new_save_point✓ rollback_to_save_point✓ release_last_save_point✓. Plus:
open/reopen, current_version, RYW, set→commit→get, overwrite-merge regression,
ScanLimit Count/Bytes/BytesOrCount, half-open range, skip+limit, pending
set/override/delete merge, nested savepoints, no-savepoint errors, versioned-false
errors, background-optimizer liveness + shutdown-timeout, concurrent disjoint +
same-key OCC, a HashMap differential property test, and Timeline (versions grow /
historical view / single-atomic-version for write+delete). tx_buffer.rs and
schema.rs carry their own unit tests.

GAPS (MINOR, recommend adding — none block the gate):
- `del` WITHOUT a prior commit (pure pending-buffer delete of a never-stored key)
  then commit: no test asserts a tombstone-only commit is a clean no-op vs a
  spurious row. (`test_delc_none_chk_on_missing_is_noop` covers delc, not del.)
- Empty-commit path (commit with an empty pending buffer → early `Ok(())` at
  mod.rs:568-571) is never directly asserted.
- `scan`/`keys` with `version=Some(_)` (historical RANGE read via `scan_impl`
  checkout) is untested — only point `get(_,Some(v))` is covered. The
  `scan_impl` versioned branch (mod.rs:1105-1108) has no test.
- `keysr`/`scanr` under a pending Delete/override are not separately asserted
  (forward scan is; reverse relies on the same merge code so risk is low).

## VERDICT
Code is compile-ready AND clippy-ready for a `--features kv-lance -- -D warnings`
run: no dangling deleted-module refs, no unused imports, no unwrap_used /
redundant_clone / single_match / needless_return / manual_ok_err hits, all
lance-6.0.0 signatures verified, arrow versions aligned. The ONE thing that can
sink the gate is running it WITHOUT `--features kv-lance` (★#1) — that yields a
meaningless green. All concrete code findings are MINOR/NIT (doc-rot + one
misleading doc clause + stale "won't compile" banner) and are optional. The
mod.rs:1068 OCC-mapping item (prior-SAVANT BLOCKER) is a correctness/idiom point,
not a clippy failure.
