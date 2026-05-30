# EPIPHANIES.md — findings log for surrealdb `.claude/`

> **APPEND-ONLY.** Newest at top. Each entry is a dated insight
> with a `**Status:**` line (FINDING / CONJECTURE / SUPERSEDED).
> Only the Status line is mutable — body and date are immutable.
> Corrections append as new dated entries citing the original.
>
> **Write only via `tee -a`** (never `Edit`, never `Write`, never
> `>` redirection). The `.claude/settings.json` enforces this.
>
> Entry template:
>
> ```bash
> tee -a .claude/board/EPIPHANIES.md > /dev/null <<'EOF'
>
> ## YYYY-MM-DD — <one-line title>
> **Status:** FINDING | CONJECTURE | SUPERSEDED-BY <new-entry-date>
> **Scope:** <area or file path>
>
> <one paragraph: what was learned, why it matters, what changes>
>
> **Cross-ref:** <pointer to commit / test / upstream issue>
> EOF
> ```
>
> **Status legend:**
> - **FINDING** — empirically verified (test ran, behaviour
>   observed, source read).
> - **CONJECTURE** — plausible but unverified; a probe is queued.
> - **SUPERSEDED** — invalidated by a later entry; keep the row.

## Entries (newest first)

## 2026-05-15 — Lance-on-SurrealDB pending-buffer pattern is a real adaptation, not boilerplate
**Status:** FINDING
**Scope:** `lance-backend/lance/tx_buffer.rs`, `lance-backend/lance/mod.rs::Transaction::commit`

Unlike SurrealKV (whose `surrealkv::Tree` has an in-tree
transactional MemTable), Lance has no per-row write buffer. The
scaffold's `PendingBuffer` + commit-time `Dataset::append` +
`Dataset::delete` is therefore the load-bearing semantic
adaptation, not an implementation convenience. Two consequences:

1. **`get` MUST check pending before the Lance scan**
   (read-your-writes inside the txn).
2. **`scan` MUST merge pending overrides AFTER materialising the
   Lance result and BEFORE applying skip/limit**, so that
   ordering is consistent across pending + stored state. This is
   spelled out in `mod.rs::scan_impl` Step 5–6 but easy to lose
   when refactoring.

Any redesign of the buffer (e.g. swap `HashMap` → `BTreeMap`)
must preserve both invariants. The two existing test fixtures
(`set_then_get_returns_set`, `set_then_delete_overrides_set`)
guard invariant 1; invariant 2 needs new range-scan tests once
`scan_impl` is wired (Day 6 in `DAY_BY_DAY.md`).

**Cross-ref:** `lance/mod.rs:362-417` (get path), `lance/mod.rs:607-642`
(scan_impl), `lance-backend/README.md` § "Transaction Model".

## 2026-05-29 — kvs-lance Timeline: Lance-native versioning IS the time-series view
**Status:** FINDING
**Scope:** surrealdb/core/src/kvs/lance/{timeline.rs,mod.rs}

The "SurrealDB-as-view-over-Lance" (Rubicon) surface needs no new storage:
Lance 6.0.0 already exposes the full timeline. `Dataset::versions() ->
Vec<Version{version:u64, timestamp:DateTime<Utc>, metadata}>` enumerates the
history; `checkout_version(u64)` pins an immutable snapshot. Confirmed against
fetched lance-6.0.0 source (dataset.rs:202 Version struct; dataset.rs:2000
versions()) AND against in-org usage in lance-graph
crates/lance-graph/src/graph/versioned.rs:432. The new `Timeline` /
`TimelineView` types are read-only BY CONSTRUCTION (they own a checked-out
snapshot, expose no set/del/commit), so "SurrealDB never mutates the leading
store" is a type-system guarantee, not a convention. Per-key time-travel
(`checkout_version` + tombstone-as-data) was already wired in get()/scan_impl();
this only adds the timeline *enumeration* + a read-only view handle. Compiles
clean under `cargo check -p surrealdb-core --features kv-lance` (Finished, 0
errors; the only warnings are never-used on the not-yet-wired consumer side).

## 2026-05-30 — kvs-lance timeline granularity = write-path-dependent (corrects 2026-05-29)
**Status:** FINDING
**Scope:** surrealdb/core/src/kvs/lance/{timeline.rs,tests.rs}

The 2026-05-29 timeline tests wrongly assumed "1 commit = 1 Lance version".
On the DEFAULT `WritePath::LsmWithWal`, commits land in WAL+memtable and the
background flusher batches them into Lance asynchronously — so the timeline
reflects FLUSH BOUNDARIES, not individual commits (observed: 2 commits → 1
version; a single commit left latest_version unchanged). For per-commit
timeline granularity (which the Rubicon kanban needs — each commit/plan/prune
a distinct entry) the datastore must use `WritePath::LegacyCommitGate`, where
`Transaction::commit` returns only after its own Lance commit lands. Tests
fixed to construct LegacyCommitGate configs; both pass (2/2). The timeline CODE
was correct; the test HARNESS used the wrong write-path. Design consequence:
the ractor/kanban consumer that publishes onto the timeline must run on the
gate path (or call an explicit flush) to get one timeline entry per Rubicon
commit. Cross-ref: config.rs WritePath docs; writepath_legacy_commit_gate_smoke.
