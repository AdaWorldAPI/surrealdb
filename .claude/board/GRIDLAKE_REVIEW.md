# GRIDLAKE_REVIEW.md — savant review of step-2 UNVERIFIED code (append-only, tee -a)

> Three read-only savant agents review the step-2 code surface (P1 adaptive
> batching, P2 WAL/ACID recovery test, P3 per-row `seq` column), authored by
> the CODE agent and NOT yet independently verified. Diff artifact:
> `/tmp/step2_code.diff`. Files: surrealdb/core/src/kvs/lance/{flusher,memtable,
> mod,schema,commit_gate,tests}.rs (+ schema.rs).
>
> **Savant rules:** READ-ONLY. NO cargo (the orchestrator is the sole cargo
> runner). NO edits to source. NO git. Append findings here via `tee -a` ONLY,
> one finding per line block, severity-tagged:
>   [S#] BLOCKER|MAJOR|MINOR|NIT <file>:<approx-line> — <issue> — <why> — <fix>
> End with `[S#] DONE — <n blockers, n major, ...> — <one-line verdict>`.

## Findings (newest at bottom)

[S2] VERIFIED schema agreement — all 5 Arrow schema decls byte-identical: mod.rs:218-224 (create-schema in Datastore::new), mod.rs:1099-1105 (build_write_batch_lance), mod.rs:1154-1160 (build_tombstone_batch_lance), schema.rs:57-64 (KvSchema::arrow_schema), schema.rs:93-96/131-134 (legacy build_*_batch). Layout in ALL: key:Binary, val:Binary, version:UInt64, tombstone:Boolean, seq:UInt64 — all non-null (false), identical ORDER. All 4 RecordBatch::try_new array-vecs push key,val,version,tombstone,seq in matching order. seq_array=UInt64Array::from(seqs.to_vec()) → non-null UInt64. No new/unstable Lance API (only MergeInsertBuilder/WhenMatched/WhenNotMatched/execute_reader).
[S2] VERIFIED seqs length-parallelism at EVERY call site — do_flush flusher.rs:307-318 pushes write_seqs/delete_seqs in lockstep with writes/deletes (seq[i] <-> key[i] by construction); commit_gate.rs:330-331 write_seqs=vec![max_version;writes.len()], delete_seqs=vec![max_version;deletes.len()] from exact partitioned lengths. Both builders carry debug_assert_eq! len guards (mod.rs:1093,1148). merge_insert keyed on "key", WhenMatched::UpdateAll overwrites ALL cols incl seq (UPDATE -> new commit seq, intended), WhenNotMatched::InsertAll. Reads unaffected: get projects ["val","version"] (mod.rs:785), scan_impl ["key","val"] (mod.rs:1236); scan_seqs_for_tests projects ["key","seq","tombstone"] (mod.rs:475), #[cfg(test)] gated. Tombstone: empty val + tombstone=true + deleting-commit seq, vectors all len deletes.len().
[S2] NIT mod.rs:1093,1148 — seqs/writes length equality enforced only by debug_assert_eq! (compiled out in release); in release a mismatch falls through to RecordBatch::try_new which DOES error (unequal col lengths) -> mapped to Error::Datastore — caught error, not UB/corruption — but no release-build assert protection. — fix: optional, try_new is the real backstop; could use checked if-return-Err for a hard guarantee. NON-BLOCKING.
[S2] NIT mod.rs:204-237 — Datastore::new opens an existing on-disk dataset without schema verification, then feeds 5-col merge_insert batches. A dataset created by a pre-seq (4-col) build would mismatch at execute_reader. Out of practical scope (kv-lance pre-release/scaffold; fresh datasets get the 5-col schema at create) so not a real break today. — fix: none now; note for a future on-disk format-version/compat gate. NON-BLOCKING.
[S2] NIT schema.rs:93-96,131-134 — legacy KvSchema::build_write_batch/build_tombstone_batch default seq=version (no per-commit seq input). #[allow(dead_code)], only called from schema.rs unit tests, NOT the live path (which uses mod.rs build_*_batch_lance). seq=version harmless+self-consistent (len matches via repeat_n). — fix: none; intent matches code. NON-BLOCKING.
[S2] DONE — 0 blocker, 0 major, 0 minor, 3 nit — Schema/Arrow correctness SOUND: all 5 schema decls + 4 RecordBatch builders agree on name/UInt64/non-null/order for seq; seqs slices length-guaranteed parallel (seq[i]<->key[i]) at both live call sites; merge_insert UpdateAll correctly overwrites seq on key-update; read projections (get/scan/keys) untouched; tombstone vectors length-matched. P3 seq column is data-model correct. Only nits (release-build assert, no on-disk schema-compat check, legacy dead-code helpers) — none blocking.

## [S3] Review — step-2 kv-lance (P1 batching / P2 recovery / P3 seq column) — 2026-05-30

Lens: Rust idiom, additive-safety, stable-only, test adequacy. Read all six
files (flusher/memtable/mod/schema/commit_gate/tests) + grepped every call site.

### Signature changes — ALL call sites verified consistent
- `build_write_batch_lance(&writes, version, &seqs)` — defs mod.rs:1081; call sites
  commit_gate.rs:386, flusher.rs:371. Both updated (3 args, correct order/types).
- `build_tombstone_batch_lance(&deletes, version, &seqs)` — def mod.rs:1139; call sites
  commit_gate.rs:392, flusher.rs:377. Both updated.
- `single_lance_commit(ds, writes, write_seqs, deletes, delete_seqs, version)` — TWO
  separate defs (flusher.rs:352, commit_gate.rs:367), each with its OWN single caller
  (flusher.rs:324 via do_flush; commit_gate.rs:333 via execute_batch). Both updated.
- `Memtable::insert_with_seq` — production callers: mod.rs:334/340 (WAL replay),
  mod.rs:1046/1049 (commit_lsm). Test-only `insert` (#[cfg(test)], memtable.rs:102)
  called ONLY from #[cfg(test)] modules (memtable.rs + nowhere in prod). Confirmed no
  production caller of the test-only `insert`. Non-test build is sound.
- `MemtableEntry { op, generation, seq }` — only struct-literal is the or_insert at
  memtable.rs:120; updated with seq. No other literal exists.
- `Transaction { .. }` literals: mod.rs:412 (transaction()) and the clone path
  mod.rs:426 both carry `commit_seq`; `Datastore` literal mod.rs:398 carries it too.
- `seq` field added to Arrow schema in ALL 4 places: create-schema mod.rs:223,
  schema.rs:62, build_write_batch_lance mod.rs:1100, build_tombstone_batch_lance
  mod.rs:1148; plus KvSchema helpers schema.rs:93/131. Consistent column order
  (key,val,version,tombstone,seq) everywhere. schema test updated (5 fields/cols).

### Stable-only / deps
- web_time::Instant used for the rate floor (flusher.rs:53); std::time only for
  Duration (flusher.rs:47, commit_gate.rs:54) — Duration is WASM-safe, not a clock.
  Tests use web_time::Instant. No std::time::Instant/SystemTime anywhere. CLEAN.
- No nightly features/attrs. Cargo.toml untouched (diff has no Cargo.toml hunk). CLEAN.

### Error handling
- New paths propagate via Error::Datastore(...) (single_lance_commit builders;
  scan_seqs_for_tests). No new .unwrap()/.unwrap_or_default()/.expect() in production
  mod.rs. pending_bytes() uses saturating_add. CLEAN.

### Verdict on the design seam (NOT a bug, recorded for the build log)
- LegacyCommitGate path stamps seq = max_version (commit_gate.rs:330-331) — documented
  and intentional; only the LSM path threads true per-commit seqs. Acceptable.
- WAL has no persisted seq; replay assigns fresh monotonic seqs in WAL order
  (mod.rs:321/331). Documented; exact pre-crash seq values are not recovered. This is a
  semantic the build log should keep visible (replay seq != original commit seq).

### Per-issue findings
[S3] MINOR mod.rs:1093,1148 — `debug_assert_eq!(writes.len(), seqs.len())` is the only length-parity guard in `build_write_batch_lance`/`build_tombstone_batch_lance`, but it is compiled out in release. — In a release build a seqs/rows length mismatch would not panic; `UInt64Array::from(seqs.to_vec())` then yields a wrong-length column and `RecordBatch::try_new` returns a generic ArrowError ("all columns must have the same length") surfaced as `Error::Datastore`. So a mismatch is still *caught* (as an opaque error, not corruption), but the clear contract message only exists in test/debug. Production callers (do_flush, execute_batch) build the slices in lockstep so it cannot actually fire. — Optional: keep the debug_assert AND add an explicit `if writes.len()!=seqs.len() { return Err(ArrowError::InvalidArgumentError(..)) }` for a self-describing release error, or leave as-is (low value). Not a blocker.

[S3] MINOR tests.rs (P1/P3 suite) — No test exercises the empty-batch or seqs-length-mismatch builder paths, and no test asserts the LegacyCommitGate stamps `seq == max_version`. — The seq-column behavior on the gate path (commit_gate.rs:330) and the builders' parallel-vec contract are unverified by tests; only the LSM path's seq threading is covered. — Add a small test that builds via the gate (WritePath::LegacyCommitGate) and asserts scanned seq == version, plus a builder test with empty inputs. Low priority; production correctness is unaffected.

[S3] NIT flusher.rs:226-239 (`should_flush`) — Doc says row threshold is "crossed (>= max_pending_rows)" and the impl uses `>=`, but the loop's prior behavior (old `memtable.len() > config.max_pending_rows`, flusher.rs:202 pre-diff) used strict `>`. The re-loop nudge at flusher.rs:251 still uses `>`. — Harmless off-by-one in threshold semantics (`>=` vs `>`): with default 1000 the flush now triggers at exactly 1000 pending rows rather than 1001. The doc and code agree, so this is intentional, but the nudge-vs-trigger asymmetry (`>` at :251 vs `>=` at :237) is a slight inconsistency. — Optionally align the re-loop nudge to `>=` for symmetry. Cosmetic.

[S3] NIT schema.rs:74 — Doc comment on `KvSchema::build_write_batch` still reads "Used by `Transaction::commit` to materialise pending writes into a single Lance append-batch", but the live commit path uses `Transaction::build_write_batch_lance` via `single_lance_commit`; the KvSchema helper is now only referenced from schema.rs's own #[cfg(test)] module. — Pre-existing stale doc, not introduced by this diff, but the diff touched this fn (added seq default) without correcting it. — Update the doc to note it is a schema-helper/test utility, or wire it in. Out of scope but cheap.

[S3] DONE — 0 BLOCKER, 0 MAJOR, 2 MINOR, 2 NIT — APPROVE. Additive & stable-only: no breaking signature leaks (all pub(super) callers updated), no new deps, no nightly, web_time honored, errors propagated. Production seq-threading (LSM) is correct and well-tested; the coalescing test is non-flaky because a 1-row notify_pending cannot pass `should_flush` (periodic=false needs a threshold cross) and back-to-back commits finish well under the 100ms tick, so shutdown's final drain yields exactly one version. Tombstone/merge_insert collapses to a single row (UpdateAll on key) so the seq assertions hold. Findings are non-blocking polish.
## [S1] Review concurrency / ACID / sequence+recovery correctness (2026-05-30)

[S1] BLOCKER mod.rs:321 commit_seq is re-initialised to AtomicU64::new(0) on EVERY Datastore::new and seeded only by counting replayed (un-flushed) WAL records (mod.rs:328-348); it is NEVER advanced past the max seq already PERSISTED in Lance, although the sibling generation counter IS advanced past max_replayed_gen (mod.rs:358-360). After any restart where the WAL was truncated (rows already flushed), commit_seq restarts from 0 and re-mints seq=1,2,3 which COLLIDE with and REGRESS BELOW the seqs (1..M) already written to Lance in the prior lifetime. The seq column's sole promised property a globally monotonic, per-commit-distinct sequence enabling order-on-seq replay (GRIDLAKE.md 5.3) is therefore BROKEN across restarts: seqs are neither unique nor monotonic over the dataset lifetime. The doc comment ("exact pre-crash seq values are not recovered") understates this: it is lost monotonicity + cross-lifetime duplicates, defeating the column's reason to exist. Fix: on open, scan Lance for MAX(seq) and initialise commit_seq to max(max_persisted_seq, replayed_count) (re-mint replayed records ABOVE that floor); or persist the per-commit seq in WalRecord and recover it on replay.

[S1] MAJOR mod.rs:331 (+218-224 schema) WAL replay assigns a FRESH seq (commit_seq.fetch_add..+1, restarting from 0) to each replayed record, bearing NO relation to that record's original seq, AND the replayed seqs collide with seqs already flushed to Lance (see BLOCKER). Even restricting to the un-flushed tail: if commits 1..5 were flushed (Lance seq 1..5) and commits 6..8 remain in the WAL, replay stamps them seq 1,2,3 interleaving with the persisted 1..5. Post-crash recovery thus renumbers seqs inconsistently with the data already in Lance, so the per-commit replay guarantee does not survive a crash. Fix: same as BLOCKER seed from persisted max and/or persist seq in the WAL.

[S1] MAJOR mod.rs:218-224 / schema.rs:62 The non-nullable seq column was added to the create-schema and both batch builders, but there is NO schema-migration path for an EXISTING on-disk Lance dataset created before this change (4 columns, no seq). LanceDataset::open (mod.rs:204) loads the old 4-col dataset; the first flush then runs a 5-col merge_insert (source carries non-null seq) against a 4-col target which Lance rejects (schema mismatch), wedging all writes on a pre-existing dataset. No test catches this because every test uses a fresh unique_tmp_path (always 5-col). Acceptable ONLY because the backend is explicitly pre-stable (Sprint II+ deferrals, no on-disk-format guarantee yet); still a real upgrade hazard. Fix: detect a missing seq column on open and add_columns (backfill seq := version) before first flush, or document the format break + bump an on-disk schema version.

[S1] MINOR mod.rs:1019-1023 generation and seq are minted from two SEPARATE atomics in commit_lsm (next_generation() then commit_seq.fetch_add), not atomically together. Under concurrent commits, tx A can take gen=5 then be preempted while tx B takes gen=6 AND its seq, leaving A with a HIGHER seq than B despite a LOWER generation i.e. seq order can disagree with commit/generation order across concurrent transactions. Per-row uniqueness still holds, and reads never consult seq (get/scan_impl project only key/val), so visibility is unaffected today; but a future replayer ordering on seq (the column's whole purpose, 5.3) would reconstruct concurrent commits in the wrong order. Fix: derive seq from generation (e.g. seq = generation) so the two cannot diverge, or mint both under one critical section. The memtable race-winner path itself is CORRECT: insert_with_seq updates seq together with generation only when generation > existing.generation, so the winning row always carries the true last-writer's seq (verified memtable.rs:113-124).

[S1] MINOR commit_gate.rs:327-333 On the LegacyCommitGate path every row is stamped seq = max_version (identical to version), so seq carries ZERO information beyond version on that path it does not survive the gate's BUNDLE key-collapse with per-commit identity (the threading work GRIDLAKE.md 5.4 calls out is not done; the gate just broadcasts the batch scalar). The column's advertised benefit (decoupling commit granularity from physical batching) is realised ONLY on the LSM path; on the gate path it is pure storage overhead and contradicts the doc's framing that seq universally provides per-commit replay fidelity. Fix: thread real per-submission seqs through execute_batch's merged map (value type Op to (Op, seq)), or document that per-commit seq fidelity is an LSM-path-only property.

[S1] MINOR tests.rs:1604 (seq_column_is_per_commit_monotonic_and_survives_coalescing) The coalescing guarantee actually rests on the row/byte THRESHOLD (a sub-max_pending_rows memtable is only flushed by a periodic 100ms tick or shutdown), NOT on "no .await that yields to the flusher" as the comment (tests.rs:1620-1624) claims there ARE several await points (transaction/set/commit) at which the current-thread runtime can schedule the flusher. The test is sound only while the two commits + shutdown complete within one 100ms tick_interval; on a slow CI disk two fsyncs could exceed 100ms, letting a periodic tick flush commit A alone -> 2 versions -> the v_after - v_before == 1 assertion fails. Low-probability flake + a rationale that mis-states why coalescing holds. Fix: set tick_interval very large (or min_flush_interval >= tick) in this test's LanceConfig so only the shutdown drain flushes, making coalescing deterministic; correct the comment to cite the threshold, not yield-timing.

[S1] MINOR tests.rs:1523 (lsm_recovery_atomic_multi_op_batch) The test proves the all-PRESENT half of atomicity (2 inserts + 1 delete from one multi-op WalRecord all visible post-recovery) but NOT the nothing half (a torn/partial record being rejected wholesale). It never constructs a partially-written record, so all-or-nothing is only half-demonstrated here; the torn-tail rejection lives in wal.rs replay tests, uncited. The name "atomic all-or-nothing" overclaims relative to what is asserted. Fix: rename to reflect "multi-op record replays together", or add a sibling asserting a truncated record tail is dropped (cross-referencing the wal.rs corruption tests).

[S1] NIT flusher.rs:277-282 vs module doc 28-36 should_flush checks the rate floor FIRST and returns false before the periodic-OR branch, so a periodic tick_interval flush IS gated by min_flush_interval too. The module doc ("The periodic tick_interval already paces timer flushes; the rate floor additionally caps row/byte-triggered flushes") implies periodic is NOT floor-gated doc/code mismatch. No starvation results because the flusher_config_defaults_are_sensible invariant min_flush_interval <= tick_interval (flusher.rs:421-424) guarantees a later tick always clears the floor (bounded delay = min_flush_interval, never indefinite), and the shutdown drain bypasses the floor (verified flusher.rs:198-202). Fix: reword the doc to state the floor gates ALL trigger-driven flushes including periodic ticks, and that the min_flush_interval <= tick_interval invariant is what bounds the delay.

[S1] DONE 1 BLOCKER, 3 MAJOR, 4 MINOR, 1 NIT seq column's core promise (cross-restart monotonic per-commit ordering) is broken: commit_seq resets to 0 each open and is never seeded from persisted Lance max, so post-restart seqs collide with/regress below flushed seqs; plus a non-nullable-column schema-migration gap. The rate-floor/adaptive-batching logic and the per-row memtable race-winner seq handling are correct; the new tests are directionally right but under-assert (atomicity tests only the commit half; coalescing test is timing-fragile).
