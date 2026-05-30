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

## 2026-05-30 — A SurrealDB commit with writes+deletes was TWO Lance versions, not one
**Status:** FINDING
**Scope:** `kvs/lance/commit_gate.rs`, `kvs/lance/flusher.rs`, `kvs/lance/mod.rs`

`single_lance_commit` applied writes via `MergeInsertBuilder::execute_reader`
and deletes via a SEPARATE `Dataset::delete` — each its own native Lance
commit. So any batch carrying both produced two versions: an intermediate
(writes applied, deletes pending) and the final. The datastore write lock hid
the intermediate from live readers, but `Timeline::versions()` enumerates raw
`Dataset::versions()` and surfaced it, letting a replayer `view_at()` a torn
state that never atomically existed. The schema was already built for the fix
(a `tombstone` Boolean column + read predicates filtering `tombstone = false`):
folding deletes as tombstone rows into the same `merge_insert` makes
1 commit = 1 version *structurally*, not by convention. Trade-off accepted:
tombstone rows accumulate until a compaction/GC pass (physical `Dataset::delete`
previously reclaimed that space immediately).

**Cross-ref:** codex P1 on PR #29 (discussion_r3328296248); fix in this
commit; regression `test_timeline_write_delete_commit_is_single_atomic_version`.

## 2026-05-30 — No CI runs on this fork; .rustfmt.toml is split-brain (stable build, nightly-only fmt opts)
**Status:** FINDING
**Scope:** repo CI/tooling — `.github/workflows/ci.yml`, `.rustfmt.toml`, `rust-toolchain*`

PR #29 head (5997eea) has ZERO check runs; the only commit status is the
CodeRabbit review bot (pending). `ci.yml` triggers on every `pull_request`
(no branch filter), so the absence is environmental: GitHub Actions is not
enabled/approved on the AdaWorldAPI fork. Net: the only merge gate is the
review bots + the human owner — there is no test/clippy/fmt enforcement.
Separately, `.rustfmt.toml` enables nightly-only options (wrap_comments,
imports_granularity=Module, group_imports=StdExternalCrate, comment_width)
while the build toolchain is pinned stable 1.95 (`rust-toolchain.toml`); the
fmt-only nightly (`rust-toolchain.nightly` = nightly-2025-08-07) is never run
here. Running fmt under either stable or that nightly reformats the WHOLE
crate (~1900 lines, 22+ files) => HEAD is not fmt-clean under its own config.
Consequence: "future-proof" config that no gate enforces is pure drift. Per
the org's 99%-stable policy (nightly only for Miri in ndarray), the resolution
is to make the config stable-honest (comment out the unstable opts) and lean
on stable tools (cargo-machete et al.). A one-time stable `cargo fmt`
normalization is a separate, deliberate follow-up (not triggered here, to
avoid mixing mass reformat churn into feature commits).

**Cross-ref:** PR #29 check status (0 runs); ci.yml on-block; this commit's
`.rustfmt.toml` change; GRIDLAKE_BUILD.md.

## 2026-05-30 — Per-commit `seq` reset to 0 on every open → broke cross-restart monotonicity (savant BLOCKER, fixed)
**Status:** FINDING
**Scope:** `kvs/lance/mod.rs` (Datastore::new seeding), step-2 P3 seq column

A 3-savant read-only review (Opus: concurrency/ACID, Lance data-model,
idiom/tests) of step-2 passed 96/0 tests yet caught a BLOCKER tests
structurally can't see: `commit_seq` was re-initialised to `AtomicU64::new(0)`
on every `Datastore::new`, seeded only from the replayed (un-flushed) WAL
record count, and NEVER advanced past the max `seq` already persisted in
Lance — whereas the sibling `generation` counter IS advanced past the
replayed-WAL max. After any restart where the WAL was truncated (data already
flushed), new commits re-minted seq=1,2,3… colliding with / regressing below
seqs already written to Lance, defeating the column's sole purpose (a globally
monotonic, per-commit replay axis). Root distinction: `generation` is
memtable-local and NOT persisted (need only clear the WAL tail); `seq` IS a
Lance column, so it must seed from the persisted max. FIX:
`Datastore::max_persisted_seq` (tolerant scan of the `seq` column; 0 for
empty/legacy) seeds `commit_seq = AtomicU64::new(max_persisted_seq)` at open;
replayed WAL records mint ABOVE that floor. Regression
`seq_survives_restart_above_persisted_max` fails on the old code, passes on the
fix. Verified: 98 kvs::lance tests pass, 0 fail.

Documented (accepted, pre-release) limitations the savants surfaced:
- No schema migration for a pre-`seq` (4-col) on-disk dataset (fresh only today).
- WAL carries no persisted seq → replay renumbers seqs above the persisted max
  (monotonic+unique), not to exact pre-crash values.
- LegacyCommitGate stamps `seq = version` (per-commit fidelity is LSM-only).
- `seq`/`generation` mint from two atomics → under concurrency seq order can
  disagree with commit order (harmless today; reads never consult seq).

**Cross-ref:** `.claude/board/GRIDLAKE_REVIEW.md` (S1/S2/S3); fix in this commit.

## 2026-05-30 — kv-lance substrate maps onto Cognitive-RISC invariants; do NOT add class_id to it
**Status:** FINDING
**Scope:** `kvs/lance/*` vs `lance-graph/.claude/specs/{cognitive-risc-core,cognitive-risc-classes,wikidata-hhtl-load,faiss-homology-cam-pq}.md`

The kv-lance backend IS the "Substrate" layer (row 1) of the Cognitive-RISC
five-layer stack ("SoA, LE byte contract, surrealkv WAL/ACID, policy-free
state"). Concrete mapping: CommitGate/single_lance_commit = the sole cold-path
writer (invariant #4); WAL+memtable ↔ flusher→Lance two-clock decoupling +
the adaptive-batching rate-floor = the shock absorber (#7); WAL carries KV
rows only, never compiled candidates (#11); the schema is opaque (key,val) +
MVCC bookkeeping version/tombstone/seq with ZERO domain meaning (#1, and #6
permits generation/tombstone counters). The step-2 `seq_survives_restart`
test is exactly the spec's "smallest first slice" (WAL round-trip + read back
after a simulated restart).

TRAP recorded so a future session does not weld the inversion shut: freeze-
time move **N1 ("add class_id/shape_id to the SoA")** must NOT be applied to
the kv-lance schema — that violates invariant #1. class_id, HHTL nibble-path,
facet bitmasks, and the CAM (BLAKE) hash live ONE LAYER UP (inside the `val`
payload or lance-graph's own Lance datasets), never as kv-lance columns. The
minimal key/val/version/tombstone/seq schema is correct precisely because it
is policy-free.

Live fork for this work — **F2**: spec default-leans "federate via DataFusion
catalog (Arrow TableProviders)", not "read Lance directly (heavy/fragile)".
kv-lance is the direct path; the step-1 Timeline ("SurrealDB-as-view-over-
Lance", Rubicon) is the federation-shaped read surface. Decide: SurrealDB as
writer-of-record into Lance (kv-lance) vs DataFusion-federated view (F2); they
can coexist but the version-coupling risk is real. Version pin skew: repo on
lance =6.0.0/arrow 58; spec pins lance 6.0.1/lancedb 0.29/datafusion 53.

**Cross-ref:** PR #29/#30; lance-graph .claude/specs/ (sha d1635db).

## 2026-05-30 — Audit: GRIDLAKE §8 Phase 1 & Phase 2 are IMPLEMENTED, not ROADMAP
**Status:** FINDING
**Scope:** surrealdb/core/src/kvs/lance/{flusher,schema,mod}.rs vs .claude/lance-backend/GRIDLAKE.md §8

A fresh-session audit of the tree on `claude/sleepy-cori-aRK2x` (HEAD 55fc45c)
against the GRIDLAKE §8 phased roadmap finds the §8 tags stale: Phase 1
(adaptive batching) and Phase 2 (per-row seq) are both shipped + tested while
§8 still reads ROADMAP / IN PROGRESS. Evidence — Phase 1: `FlusherConfig`
carries the byte trigger `max_pending_bytes` (flusher.rs:74, default 8 MiB)
and the rate floor `min_flush_interval` (flusher.rs:79, default 50 ms);
`should_flush` (flusher.rs:271-285) gates trigger-driven + periodic flushes on
the floor; locked by `should_flush_triggers_on_bytes` (flusher.rs:441) and
`should_flush_respects_rate_floor` (flusher.rs:460). Phase 2: `seq: UInt64` is
the 5th schema column (schema.rs:62), threaded through both batch builders with
parallel-length assertions (mod.rs:1180 build_write_batch_lance, mod.rs:1242
build_tombstone_batch_lance); `commit_seq` (mod.rs:177) seeds from the on-disk
max via `max_persisted_seq` (mod.rs:203-239) — the savant BLOCKER fix — and
55fc45c fails fast on a legacy 4-column dataset. `cargo check -p surrealdb-core
--features kv-lance --tests` → 0 errors, 6 dead-code warnings (unwired
TimelineView consumer); prior run 98 kvs::lance tests pass. §8 should read
IMPLEMENTED for Phases 1-2. Per the append-only discipline the GRIDLAKE doc
body was NOT mutated; this entry is the canonical re-tag record.

**Cross-ref:** commits 7266acf, e329a7a, 55fc45c; GRIDLAKE.md §8; this session's grep/check audit.

## 2026-05-30 — Next-arc integration plans located + consolidated (BindSpace→SoA / AriGraph witness / ractor mailbox)
**Status:** FINDING
**Scope:** cross-repo; recorded in .claude/board/INTEGRATION_PLANS.md

The three next items already have concrete plans, as a PR series in
**lance-graph/.claude/specs/ (the `pr-ce64-mb-*` CausalEdge64-MailBox series)**:
mb-3 BindSpace-EFGH (A), mb-4 AriGraph-SPO-G + sprint-13 witness-CAM-PQ (B),
mb-5 mailbox-SoA-attentionmask + pr-f-1/g2 ractor-supervisor (C). SoA carrier
math is ndarray's 3DGS-4x4-cognitive-shader-SoA plan (`BindSpace4` lanes);
AriGraph is SHIPPED (lance-graph graph/arigraph, ~4.7k LOC); witness ingestion
is `witness→splat→RowDelta→apply` (lance-graph pattern.md:236). Substrate is
the just-merged kv-lance + Rubicon Timeline (#31). KEY: #31's "one commit = one
version" retires the old "kanban consumer must run on the gate path" caveat —
per-card-move timeline granularity is now free. INVARIANT preserved: AriGraph
witness/class metadata go ONE LAYER UP (val payload / lance-graph datasets),
never into kv-lance columns (N1). Open forks: replace-vs-migrate BindSpace;
EpisodicWitness64-onto-timeline design; writer-of-record vs DataFusion view (F2);
lance 6.0.0 vs 6.0.1 pin skew. Full matrix + cross-repo diagram in INTEGRATION_PLANS.md.

**Cross-ref:** PR #31; lance-graph/.claude/specs/pr-ce64-mb-{3,4,5}; ndarray 3DGS-4x4-SoA plan; AGENT_LOG.md:96-97.

## 2026-05-30 — CORRECTION: q2 is conjecture; removed from INTEGRATION_PLANS; AriGraph "shipped" retracted
**Status:** CORRECTION (supersedes the q2 + AriGraph-SHIPPED claims in the
"2026-05-30 Next-arc integration plans located + consolidated" finding above).

Per user: **q2 is conjecture.** All q2 references have been removed from
INTEGRATION_PLANS.md — q2 had been cited as the kanban UI (item C), the
provenance consumer (item B), and a BindSpace driver (item A), none verified
against the q2 repo. Consequently the earlier finding's claim that **AriGraph
is SHIPPED (~4.7k LOC, graph/arigraph/)** is **RETRACTED**: that figure was
sourced only from a q2 knowledge doc. AriGraph's grounded status is now
**SPEC'D in lance-graph** (pr-ce64-mb-4-arigraph-spo-g); whether the module is
already shipped must be verified directly in the lance-graph repo. The Rubicon
kanban UI/renderer (item C) is now **TBD**, not q2. Everything else stands: the
lance-graph pr-ce64-mb-* specs, the ndarray SoA carrier, the ractor primitive,
and the kv-lance/#31 substrate are unaffected by this correction.
**Cross-ref:** INTEGRATION_PLANS.md (corrected this session); AGENT_LOG.md:96-97.

## 2026-05-30 — CORRECTED ARCHITECTURE: per-mailbox triple + pointer table; ractor owns Rubicon phases; EW64+CE64
**Status:** FINDING / DESIGN-direction (from user, 2026-05-30). Supersedes the
*framing* of the earlier "Next-arc integration plans" finding (the plan items
A/B/C still hold, but the backbone below is the corrected model).

- **q2 is OUT OF SCOPE** — it's a separate OSINT-harvesting scaffold (aspiring to
  a Palantir-Foundry shape), unrelated to this arc. Do not wire it in.
- **Per-mailbox TRIPLE (1:1:1):** each ractor mailbox owns one BindSpace SoA + one
  kanban. A **pointer table** meta-coordinates the {mailbox, SoA, kanban} triples
  as **O(1)** references.
- **Kanban substrate = ractor mailbox + surrealdb** (kv-lance / Rubicon timeline),
  NOT q2. Per #31, one card-move = one commit = one version (free).
- **ractor owns the Rubicon PHASES** ("übernimmt die Phasen"): each phase
  transition = one commit on the timeline = one kanban move.
- **Planning:** actor model needs an expansive **pre-planning phase**; the
  collapsed **final plan runs JIT-adjacent** in the mailbox or as
  **SurrealQL→elixir-like templates**, inside cognitive-shader-strategy
  orchestration. **lance-graph-planner must become a DTO** wired to ractor + the
  surrealdb kanban.
- **SoA rows:** CausalEdge64 (existing, the pr-ce64-mb-* series) + **EpisodicWitness64
  (NEW)** → synergies.
- **AriGraph SPO is partially disconnected** → fix by wiring into **EpisodicWitness64
  inside the SoA**, fed by BOTH the mailboxes (hot) and cold-path SPO/AriGraph
  facts. (N1: EW64 payload one layer up, not in kv-lance columns.)
- **Interpretation flags (need confirmation):** the "SurrealQL→elixir-like
  templates" layer + the `>` meaning; pointer-table location; pre-plan→JIT
  collapse boundary. Full model + open decisions in INTEGRATION_PLANS.md (rev 2026-05-30).
**Cross-ref:** INTEGRATION_PLANS.md (rev); supersedes framing of the two prior 2026-05-30 findings.

## 2026-05-30 — REFINEMENT: keep the kanban on ractor's HOT path, off the Tokio message path
**Status:** DESIGN refinement (user nudge, 2026-05-30).
ractor delivers messages over Tokio (per-message heap box + mpsc send + task
wake) — too expensive for the per-update hot path. Wire the kanban onto the
**hot path** alongside the SoA: since {mailbox, SoA, kanban} is one co-owned
per-mailbox triple, the actor mutates SoA + kanban together in-process (O(1) via
the pointer table / a shared lock-free snapshot), with **no extra ractor message
per card-move**. ractor/Tokio messages are reserved for control-plane +
cross-mailbox coordination; the durable Rubicon commit is batched at the phase
boundary (one version per phase transition, not per message). Kanban thus has two
faces: a hot in-memory projection (no Tokio cost) + a durable Rubicon-timeline
projection. Recorded in INTEGRATION_PLANS.md §1 (Hot path bullet) + §5(8).
**Cross-ref:** INTEGRATION_PLANS.md (rev 2026-05-30).
