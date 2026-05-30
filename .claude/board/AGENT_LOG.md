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
