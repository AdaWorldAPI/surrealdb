# INTEGRATION_PLANS — next-arc roadmap (consolidated 2026-05-30)

> Consolidated by the kv-lance native-rewrite session (after PR #31 merged).
> Cross-repo plan index for the three next-in-line items. Citations are
> `repo:path[:line]`. Status legend: SHIPPED / SPEC'D (PR spec exists, not built)
> / INTENT (mention only, no detailed plan) / MERGED.

## Substrate baseline (what just landed — the floor everything sits on)
- **surrealdb#31 MERGED** — kv-lance is now *native* lance read/write: one
  `MergeInsert` per commit = **one lance dataset version**; reads via
  `checkout_version`/`scan`; compaction via lance `optimize`. Schema is
  policy-free `key,val,version,tombstone,seq`.
  Cognitive-RISC mapping: kv-lance = **Substrate (row 1)** of the 5-layer stack
  (surrealdb:.claude/board/EPIPHANIES.md "kv-lance substrate maps onto
  Cognitive-RISC"). The read-only `Timeline` over `Dataset::versions()` is the
  **"Rubicon"** federation-shaped read surface (surrealdb:core/src/kvs/lance/timeline.rs).
- **Consequence for item C:** the old caveat "the kanban consumer must run on
  the gate path to get one timeline entry per commit" (EPIPHANIES 2026-05-30
  timeline-granularity) is now **moot** — the native path makes *every* commit
  one version, so per-card-move granularity is free.
- **INVARIANT (do not violate):** N1 "add class_id/shape_id to the SoA" must
  NOT touch the kv-lance schema. class_id / HHTL nibble-path / facet bitmasks /
  CAM(BLAKE) hash live **one layer up** — inside the `val` payload or
  lance-graph's own Lance datasets. kv-lance stays policy-free.

---

## A. BindSpace → SoA migration
**Status:** SPEC'D (substrate) + INTENT (replacement).
**Lives in:** ndarray (SoA carrier math), lance-graph (BindSpace columns + driver), q2 (current BindSpace driver), surrealdb (substrate).
**Plans that exist:**
- `lance-graph:.claude/specs/pr-ce64-mb-3-bindspace-efgh.md` — BindSpace EFGH (the SoA-column expansion).
- `ndarray:.claude/plans/3DGS-4x4-cognitive-shader-SoA-plan.md` — the **`BindSpace4`** 4-lane SoA carrier (lane0 id, lane1 covariance/edge, lane2 confidence, lane3 time/phase/provenance) + `(4x4)^4` block fanout. This is the numeric substrate BindSpace's columns migrate onto.
- `surrealdb:.claude/board/AGENT_LOG.md:96-97` — "**replace BindSpace**; wire deprecated→cognitive-shader-driver".
- `ndarray:.claude/prompts/05_cross_repo_map.md:88-101` — consumer migration tracked (ladybug-rs, crewai-rust use rustynum *via BindSpace*; graph-flow-memory crate planned as AriGraph schema port).
- Today BindSpace IS the universal DTO (~2087 lines, in cognitive-shader-driver; `q2:.claude/reference/aiwar/INTEGRATION_PLAN_SCHEMA_CHANGES.md`).
**Gaps/forks:** "migrate BindSpace's columns onto BindSpace4 SoA" vs "replace BindSpace with kv-lance substrate" are two different endpoints — reconcile. Version-pin skew: surrealdb on lance 6.0.0/arrow 58; lance-graph specs pin lance 6.0.1/lancedb 0.29/datafusion 53.

## B. AriGraph as a witness arc in the SoA
**Status:** AriGraph SHIPPED; witness-arc onto-SoA SPEC'D + INTENT.
**Lives in:** lance-graph (AriGraph + witness), surrealdb (timeline = the arc), q2 (provenance consumer).
**Plans that exist:**
- AriGraph is **SHIPPED** in lance-graph at `crates/lance-graph/src/graph/arigraph/` (~4,696 lines; coordinator `orchestrator.rs`) — `q2:.claude/knowledge/chess-nars-vertical-slice.md:51-61`.
- `lance-graph:.claude/specs/pr-ce64-mb-4-arigraph-spo-g.md` — AriGraph SPO-G (the quad/graph form).
- `lance-graph:.claude/specs/pr-sprint-13-witness-cam-pq.md` — witness + CAM-PQ.
- Witness ingestion pattern is defined: **`witness → splat → RowDelta → apply()`** (`lance-graph:.claude/pattern.md:236`).
- `surrealdb:.claude/board/AGENT_LOG.md:97` — "**EpisodicWitness64**" (attach episodic-memory witness to the SoA/timeline).
- AriGraph already used as a **provenance label** in q2 (`source:"AriGraph"`, `q2:crates/cockpit-server/src/scene_player.rs:150`).
**Gaps:** "AriGraph as a *witness arc on the Rubicon version-timeline*" (EpisodicWitness64) is **INTENT** — no concrete surrealdb-side design yet for how an episodic-witness edge attaches to `Dataset::versions()` entries. Per invariant N1, the witness payload (CAM hash, SPO-G quad) goes one layer up (in `val`/lance-graph datasets), NOT into kv-lance columns.

## C. ractor mailbox "Rubicon kanban + mailbox SoA"
**Status:** SPEC'D (mailbox SoA + ractor supervisor); substrate MERGED; consumer not built.
**Lives in:** lance-graph (mailbox-SoA + ractor supervisor), ractor (actor/mailbox primitive), surrealdb (Rubicon timeline = publish target), q2 (kanban UI).
**Plans that exist:**
- `lance-graph:.claude/specs/pr-ce64-mb-5-mailbox-soa-attentionmask.md` — **mailbox SoA** + attention mask.
- `lance-graph:.claude/specs/pr-f-1-ractor-supervisor.md` + `pr-g2-ractor-supervisor.md`; pattern F "ractor/BEAM supervisor, design shape-proven" at `crates/cognitive-shader-driver/src/grpc.rs` (`lance-graph:.claude/patterns.md:434,611`).
- `surrealdb:.claude/board/AGENT_LOG.md:96` — "**ractor mailbox owns SoA → publishes link onto this timeline (kanban)**".
- ractor primitive: `ractor:ractor/src/port.rs`, `ractor:docs/runtime-semantics.md` (mailbox semantics).
- Kanban UI exists in q2 (`q2:q2-demos/kanban/*`, `q2:crates/cockpit-server/src/{shader_stream,scene_player}.rs`).
**Gaps:** the ractor mailbox that *owns the SoA and drives the Rubicon timeline* is **not built**. With #31, the per-commit granularity it needs is now free (every commit = one version), so the build reduces to: ractor mailbox actor → on each card-move, one kv-lance commit (= one timeline/kanban entry) → q2 renders the timeline. Decide writer-of-record (kv-lance direct, the merged path) vs DataFusion-federated view (the F2 fork, EPIPHANIES 2026-05-30).

---

## Cross-repo map (how A/B/C interlock)
```
q2 kanban UI ─────────────────────────────────┐ (renders)
                                               ▼
ractor mailbox (owns SoA) ──commit per move──▶ surrealdb kv-lance  ──one version──▶ Rubicon Timeline   [C, substrate=MERGED #31]
        ▲ owns                                   (Substrate row 1, policy-free)        ▲ witnessed by
        │                                                                              │
ndarray BindSpace4 SoA carrier  ◀──migrate cols── BindSpace (cognitive-shader-driver)  AriGraph EpisodicWitness64
        [A: SoA math, SPEC'D]              [A: replace, INTENT]                         [B: witness arc, SHIPPED+INTENT]
                                                                                        (witness→splat→RowDelta→apply)
```
Canonical spec home: **lance-graph/.claude/specs/pr-ce64-mb-* (the CE64-MailBox series)**. Substrate home: **surrealdb kv-lance + timeline (#31)**. SoA-math home: **ndarray**. UI: **q2**.

## Open decisions blocking the build
1. BindSpace endpoint: migrate-onto-BindSpace4 vs replace-with-kv-lance (or both, layered).
2. EpisodicWitness64 design: how an AriGraph witness arc attaches to a `Dataset::versions()` entry (one layer up from kv-lance columns, per invariant N1).
3. Writer-of-record vs DataFusion-federated view (F2 fork).
4. Version-pin skew lance 6.0.0 (surrealdb) vs 6.0.1/lancedb 0.29 (lance-graph specs).
