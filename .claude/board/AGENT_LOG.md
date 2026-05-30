# AGENT_LOG.md — surrealdb `.claude/` session log

> **APPEND-ONLY.** Newest at top. Every agent run / session that
> touches `.claude/` or `surrealdb/core/src/kvs/lance/` appends one
> entry on completion. Old entries never mutate, including their
> Status field — corrections append as new dated entries that cite
> the original.
>
> **Write only via `tee -a`** (never `Edit`, never `Write`, never
> `>` redirection). The `.claude/settings.json` enforces this for
> board files.
>
> Entry template:
>
> ```bash
> tee -a .claude/board/AGENT_LOG.md > /dev/null <<'EOF'
>
> ## YYYY-MM-DDTHH:MM — <agent-name-or-session-tag>
> **Branch:** <branch>
> **Scope:** <files touched, max 5 paths>
> **Verdict:** <PASS / BLOCK / DEFERRED / CONJECTURE>
>
> **What was done (max 5 lines):**
> - …
>
> **Tests run:**
> - `<cmd>` → <result>
>
> **Open questions / follow-ups:**
> - …
>
> **Commit(s):** <sha[, sha…]>
> EOF
> ```
>
> Mandatory fields: agent-name, Branch, Scope, Verdict, Commit(s).
> Optional fields drop the heading line when empty.

## Entries (newest first)

## 2026-05-15T00:00 — knowledge-base-bootstrap
**Branch:** `claude/setup-knowledge-base-VWNi7`
**Scope:**
- `.claude/CLAUDE.md`
- `.claude/BOOT.md`
- `.claude/board/AGENT_LOG.md` (this file, seeded)
- `.claude/board/EPIPHANIES.md` (seeded)
- `.claude/knowledge/{lance-api-surface,transactable-contract}.md`
- `.claude/hooks/session-start.sh`
- `.claude/settings.json`

**Verdict:** PASS (scaffold only — no code touched)

**What was done:**
- Established the `.claude/board/` + `.claude/knowledge/` + `.claude/hooks/` directories that lance-graph and ndarray use, scoped down to what's useful for the lance-backend POC (one project, one path).
- Seeded `AGENT_LOG.md` and `EPIPHANIES.md` with append-only discipline + entry templates.
- Added `lance-api-surface.md` (Lance Dataset/Transaction surface the TODO sites in `mod.rs` will call) and `transactable-contract.md` (the 19-method contract the implementation must honour).
- Wired `session-start.sh` to inject the read order at turn 0, and `settings.json` to enforce `tee -a` on board files.
- Updated `CLAUDE.md` directory tree and `BOOT.md` step-1 reads.

**Tests run:** none (docs/scaffold only).

**Open questions / follow-ups:**
- The Lance-API reference in `lance-api-surface.md` is pinned to the
  TODO comments in `lance/mod.rs`; if the actual `lance` crate
  version chosen by `Cargo-toml.patch.txt` exposes different
  signatures, prefer the actual crate docs (`cargo doc --open
  --package lance`) and append a correction entry here.
- No `agents/` directory yet — one specialist card (e.g.
  `lance-integrator`) can be added when a session first delegates
  TODO(lance-integration) work to a subagent. Premature now (only
  one work path).

**Commit(s):** _filled in by the committing session_

## 2026-05-29 — kvs-lance time-series view (SoA/Rubicon step 1)
**Branch:** claude/sleepy-cori-aRK2x
**Added:**
- `kvs/lance/timeline.rs` (264 LOC) — `Timeline` (read-only view over Lance
  version history) + `TimelineView` (immutable snapshot at one version) +
  `VersionInfo{version:u64, timestamp_us:Option<i64>}`. Uses only confirmed
  Lance 6.0.0 surface: versions(), checkout_version(), version().version,
  scan().project()/filter(). Tombstone-aware reads.
- `kvs/lance/mod.rs` — `Datastore::timeline()` accessor (shares the live
  dataset handle, no second open).
- `kvs/mvcc_source.rs` (170 LOC) — `MvccSource` trait + `LocalGeneratedMvcc`,
  borrowed verbatim from reverted PR #24 (2a54a32); additive, dead_code-gated
  until its consumer (kv-tikv native MVCC / lance version source) lands.
- `kvs/lance/tests.rs` — 2 tests: versions grow+monotone with commits; a
  historical TimelineView reads the SoA as it stood (present at write version,
  absent before).
**Verify:** `cargo check -p surrealdb-core --features kv-lance` → Finished, 0
errors (6m43s cold). Timeline tests: see commit (run pending at log time).
**Deferred (per user):** thinking-style i4-32 `I4x32::pack/unpack` are todo!()
in lance-graph-contract (carrier glitch) — NOT touched; wiring first.
**Next:** ractor mailbox owns SoA → publishes link onto this timeline (kanban);
EpisodicWitness64; replace BindSpace; wire deprecated→cognitive-shader-driver.

## 2026-05-30 — codex P1 fix: write+delete commit = ONE Lance version (PR #29)
**Branch:** claude/kvs-lance-timeline
**Scope:**
- `kvs/lance/commit_gate.rs`, `kvs/lance/flusher.rs` — `single_lance_commit`
- `kvs/lance/mod.rs` — `build_tombstone_batch_lance` helper
- `kvs/config.rs` — retire dead `delete_via_tombstone_row` flag
- `kvs/lance/tests.rs` — regression test
**Verdict:** PASS

**What was done (max 5 lines):**
- Codex P1 on PR #29 was VALID: a batch with BOTH writes and deletes ran
  `merge_insert` (writes) THEN `Dataset::delete` (deletes) = TWO Lance
  versions; the intermediate write-before-delete version leaked through
  `Timeline::versions()` as a snapshot that was never an atomic commit.
- Fix: fold deletes into tombstone rows (`tombstone=true`) in the SAME
  `merge_insert` → exactly ONE version per commit/flush. New
  `Transaction::build_tombstone_batch_lance` mirrors `build_write_batch_lance`.
- Read path already filters `tombstone = false` (schema.rs:145,152), so
  get/scan/keys stay correct; physical `Dataset::delete` fully removed.
- Retired the never-read `delete_via_tombstone_row` config flag — the fix is
  unconditional; a toggle would only re-open the torn-timeline hole.

**Tests run:**
- `cargo check -p surrealdb-core --features kv-lance` → Finished, 0 errors
- `cargo test … kvs::lance::tests::test_timeline` → 3 passed; 0 failed (incl.
  new `test_timeline_write_delete_commit_is_single_atomic_version`)

**Open questions / follow-ups:**
- Tombstone rows now accumulate (one dead row per created-then-deleted key)
  until compaction; the background optimizer should GC tombstones past the
  retention horizon — queued for the compaction pass.
- NOT run through `cargo +nightly fmt`: the crate is not fmt-clean under
  `.rustfmt.toml`'s unstable opts (whole-crate churn across 22+ untouched
  files), so hand-formatted to match surrounding `lance/` style.

**Commit(s):** (this commit)

## 2026-05-30 — Step-2 gridlake: orchestrated build + savant review + BLOCKER fix
**Branch:** claude/sleepy-cori-aRK2x
**Scope:**
- `kvs/lance/{mod,flusher,schema}.rs`, `kvs/config.rs`, `kvs/lance/tests.rs`
- `.claude/lance-backend/GRIDLAKE.md`, `.rustfmt.toml`, board logs
**Verdict:** PASS

**What was done (max 5 lines):**
- Orchestrated Opus agents via file-based A2A (tee -a logs): DOC → 800-line
  GRIDLAKE architecture; CODE → P1 adaptive batching, P2 WAL atomic-recovery
  test, P3 per-row `seq` column.
- 3 read-only savants (no cargo; orchestrator sole cargo runner) reviewed the
  diff: S2/S3 clean, S1 found 1 BLOCKER + 3 MAJOR + 4 MINOR + 1 NIT.
- Fixed BLOCKER (seq seeded from persisted Lance max), real length checks,
  `flusher_tick_interval` knob + deterministic coalescing test, NITs; +2
  regression tests; documented accepted limitations.
- `.rustfmt.toml` made stable-honest (org 99%-stable policy).

**Tests run (orchestrator):**
- `cargo check --features kv-lance --tests` → Finished, 0 errors
- `cargo test --features kv-lance --lib kvs::lance` → 98 passed, 0 failed, 3 ignored

**Open questions / follow-ups:**
- Schema migration for pre-`seq` datasets; persist seq in WAL for exact replay;
  per-commit seq on the gate path; max-seq via manifest metadata not a scan.

**Commit(s):** (this commit)

## 2026-05-30T14:40 — phase3-step1 (full-auto session)
**Branch:** claude/sleepy-cori-aRK2x
**Scope:**
- surrealdb/core/src/kvs/config.rs (WritePath::LsmColumnar variant)
- surrealdb/core/src/kvs/lance/mod.rs (exhaustive write_path dispatch)
- surrealdb/core/src/kvs/lance/tests.rs (writepath_lsm_columnar_smoke)
**Verdict:** PASS

**What was done (max 5 lines):**
- Added the opt-in `WritePath::LsmColumnar` variant (GRIDLAKE §8 Phase 3 seam).
- Wired it through every write_path match in mod.rs via or-patterns with
  LsmWithWal (commit dispatch, get + scan_impl snapshot selection, the two
  read-path `== LsmWithWal` checks) — currently aliases the proven LSM hot path.
- The single-pass columnar flush builder lands in step-2 behind this seam.

**Tests run:**
- `cargo check -p surrealdb-core --features kv-lance --tests` → Finished, 6 pre-existing warnings
- `cargo test -p surrealdb-core --features kv-lance --lib kvs::lance` → 99 passed; 0 failed; 3 ignored

**Open questions / follow-ups:**
- Step-2: FlusherConfig.columnar flag → do_flush branch → build_columnar_merge_batch
  (single up-front-sized Arrow builder pass) + extracted execute_merge; parity test.

**Commit(s):** 00f0e12

## 2026-05-30T14:58 — phase3-step2 (full-auto session)
**Branch:** claude/sleepy-cori-aRK2x
**Scope:**
- surrealdb/core/src/kvs/lance/flusher.rs (columnar flag, do_flush branch,
  execute_merge extraction, build_columnar_merge_batch)
- surrealdb/core/src/kvs/lance/mod.rs (spawn columnar flag)
- surrealdb/core/src/kvs/config.rs (LsmColumnar doc)
- surrealdb/core/src/kvs/lance/tests.rs (writepath_lsm_columnar_flush_persists)
**Verdict:** PASS

**What was done (max 5 lines):**
- LsmColumnar flush now builds the merge source in ONE up-front-sized Arrow
  columnar pass over the snapshot (build_columnar_merge_batch) — one fused
  batch (live + tombstone rows), no row-vec partition, no two-batch concat.
- FlusherConfig.columnar flag (set from write_path at spawn) branches do_flush;
  MergeInsertBuilder execution extracted into shared execute_merge.
- Row path unchanged + default. memtable/WAL stay row-oriented (full SoA = future).

**Tests run:**
- `cargo test -p surrealdb-core --features kv-lance --lib kvs::lance` → 100 passed; 0 failed; 3 ignored

**Open questions / follow-ups:**
- GRIDLAKE §6.2 native-Arrow memtable+WAL (SoA, CONJECTURE) still open — this
  step is the flush-side columnar build only, not a natively columnar memtable.
- Phase 4 (tombstone GC + version backpressure) is the next roadmap item.

**Commit(s):** d9bfca7

## 2026-05-30T15:30 — phase3 clippy hygiene (full-auto session)
**Branch:** claude/sleepy-cori-aRK2x
**Scope:** surrealdb/core/src/kvs/lance/mod.rs (get, scan_impl)
**Verdict:** PASS

**What was done (max 5 lines):**
- Ran clippy on the kv-lance surface (9m34s); exit 0. My Phase 3 changes
  introduced ZERO new lints — the 3 it cited are pre-existing (verified vs
  348bb4d), just inside the read-path fns I edited for LsmColumnar.
- Cleared them with clippy's verbatim fixes: get() nested if-let → let-chain;
  scan_impl() two `match {Ok=>Some,Err=>None}` → `.ok()`. Behaviour-identical.

**Tests run:**
- `cargo test -p surrealdb-core --features kv-lance --lib kvs::lance` → 100 passed; 0 failed; 3 ignored

**Open questions / follow-ups:**
- 6 clippy warnings remain: unwired TimelineView dead-code (prior session,
  intentional). `-D warnings` can't pass until that consumer lands — out of scope.
- Integration suite (SURREAL_TEST_KV=lance) NOT run: full-workspace build risks
  ENOSPC (13G free); needs more reclaim or a scoped run.

**Commit(s):** (this commit)

## 2026-05-30T16:10 — kv-lance NATIVE REWRITE: orchestration start (full-auto)
**Branch:** claude/sleepy-cori-aRK2x
**Scope:** .claude/lance-backend/specs/{00,10,20,30}-*.md
**Verdict:** IN PROGRESS
**Plan:** delete hand-rolled LSM (memtable/wal/flusher/commit_gate + WritePath);
rewire kv-lance to native lance read/write (MergeInsert commit, checkout_version
reads, lance optimize) — same path lance-graph uses. Pipeline: 1 Opus agent/file
(mod.rs, config.rs, tests.rs) → savant testers → fix → strip `// ///REVIEW:` →
clippy (sole gate) → PR → subscribe+fix. tee-only, no compiles until clippy.
- 2026-05-30 AGENT 2 (config.rs): LanceConfig flusher cleanup. Found WritePath enum + write_path/flusher_tick_interval/disable_background_flusher already absent from current config.rs; LanceConfig already carried only `versioned`. Verified no WritePath refs/uses remain anywhere. Updated LanceConfig doc-comment to record the native rewrite drops those knobs (lance optimize owns compaction/GC). Left a // ///REVIEW: noting spec KEEP list mentions retention_ns but the struct never had it (env-var/background_optimizer owns retention) — did not invent a field. Touched ONLY the kv-lance LanceConfig region via tee; Memory/SurrealKv/RocksDb configs untouched.

## 2026-05-30 — AGENT 1: native-only kvs/lance/mod.rs
Rewrote `surrealdb/core/src/kvs/lance/mod.rs` to lance's NATIVE read/write path.
commit() now builds one Arrow batch (writes + tombstone rows) and applies it with a
single `MergeInsertBuilder::execute_reader` (moved `execute_merge` in from flusher.rs)
= one commit / one lance version. Deleted commit_lsm/commit_legacy_gate and the
memtable branch in get()/scan_impl(); reads are pending → lance (checkout_version via
.ok() | latest) → filter/project/merge. Datastore now keeps {dataset, versioned,
background_optimizer, commit_seq}; Transaction keeps {done, write, versioned, pending,
save_points, read_version, dataset, background_optimizer, commit_seq}. Removed
mod {commit_gate,flusher,memtable,wal} + their imports + WritePath. Kept proven helpers
build_write_batch_lance/build_tombstone_batch_lance/max_persisted_seq/DatasetHandle and
the test-only scan helpers. NOTE: tests.rs + integration_tests still reference removed
items (WritePath, LanceConfig::{write_path,disable_background_flusher,flusher_tick_interval},
commit_gate module) — they need agent 2/3 / orchestrator follow-up. Did NOT run cargo.

## 2026-05-30T17:05 — AGENT 3: rewrote kvs/lance/tests.rs (Transactable contract only)
**Scope:** surrealdb/core/src/kvs/lance/tests.rs (tee-only; no cargo run)
- Kept 57 `#[tokio::test]` contract tests: lifecycle (kind/closed sticky/writeable/readonly+finished guards), get/exists (RYW, pending tombstone, committed, versioned-on-false errors), set/commit/cancel/overwrite/del round-trips, put/putc/delc CAS (match/mismatch/None-None/None-on-existing), scan/scanr/keys/keysr (order, half-open range, skip+limit, pending merge incl. override+delete-hide), ScanLimit Bytes/BytesOrCount, savepoints (rollback incl. tombstone restore, release, nested, empty-stack errors for both), versioning time-travel, concurrency (disjoint + same-key OCC), differential-vs-HashMap, optimizer-alive + shutdown-timeout, 3 timeline read-view tests.
- Deleted all LSM/reinvention tests: writepath_*, lsm_recovery_*, seq_column_*, commit_gate_*, shutdown_drains_pending_commits, bench_lsm_*; dropped `WritePath` import + `scan_seqs/scan_versions/dataset_for_tests` usage.
- LanceConfig field set ASSUMED = `{ versioned: bool }` only (matches the already-rewritten config.rs); every literal now sets only `versioned`.
- 4 `// ///REVIEW:` anchors (all about "one commit = one lance version"): get_at_specific_version (checkout sees old-or-None), timeline versions_grow (≥2 lower-bound vs compaction), timeline view historical (v_after>v_before), timeline write+delete single-version (==before+1).

## 2026-05-30T16:55 — agents 1+3 landed; orphans + integration_tests removed
**Branch:** claude/sleepy-cori-aRK2x
**Scope:** kvs/lance/{mod.rs(native),tests.rs(57 contract tests)}; deleted
  memtable/wal/flusher/commit_gate/integration_tests.rs + WritePath.
**Verdict:** IN PROGRESS (native source coherent; 8 // ///REVIEW anchors open)
**Open:** REVIEW anchors — version stamp (read_version+1 vs latest+1),
  lance OCC conflict -> Error::TransactionRetryable, get@version deletion-vector
  semantics, timeline version-count assertions. Next: savant testers -> fix ->
  strip /// -> clippy -> PR -> subscribe.
