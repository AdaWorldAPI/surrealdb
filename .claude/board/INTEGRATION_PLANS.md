# INTEGRATION_PLANS — next-arc roadmap (rev. 2026-05-30)

> Consolidated by the kv-lance native-rewrite session (after PR #31 merged),
> then **revised to the corrected architecture** (design direction, 2026-05-30).
> Citations are `repo:path[:line]`. Status legend: SHIPPED / SPEC'D (PR spec
> exists, not built) / INTENT / MERGED / DESIGN (stated direction, not yet spec'd).
>
> **SCOPE — q2 is OUT OF SCOPE here.** q2 is a separate scaffolding for an
> OSINT-harvesting crate (aspiring to a Palantir-Foundry shape). It has nothing
> to do with the cognitive-substrate arc below — do **not** wire q2 into these
> items, and do not cite it as the UI/consumer.

---

## 0. Substrate baseline (#31) — the floor, unchanged
- **surrealdb#31 MERGED** — kv-lance is native lance: one `MergeInsert` per
  commit = **one lance dataset version**; reads via `checkout_version`/`scan`;
  compaction via `optimize`. Schema policy-free `key,val,version,tombstone,seq`.
  The read-only `Timeline` over `Dataset::versions()` is the **Rubicon** read
  surface (`surrealdb:core/src/kvs/lance/timeline.rs`).
- **Consequence:** every commit = one version, so "one kanban entry per move"
  is **free** — no gate-path special-casing needed.
- **INVARIANT N1:** class_id / shape_id / HHTL path / facet bitmasks / CAM(BLAKE)
  and all EW64/SPO payload live **one layer up** (in the `val` payload or
  lance-graph datasets), **never** in the kv-lance columns. kv-lance stays
  policy-free.

---

## 1. Core architecture (the backbone) — DESIGN direction 2026-05-30
The unit of organization is the **per-mailbox triple**, meta-coordinated by a
pointer table:

```
        ┌──────────────── meta-coordination ────────────────┐
        │     POINTER TABLE  —  O(1) ref:  id → triple        │
        └───────────────────────┬─────────────────────────────┘
                                 │ indexes every triple (1:1:1)
   ┌─────────────────────────────────────────────────────────────┐
   │  ractor MAILBOX   ⟷   BindSpace SoA   ⟷   KANBAN              │   one per mailbox
   │  (actor/hot path)     (per mailbox)       (per mailbox)        │
   └─────────────────────────────┬───────────────────────────────┘
                                 │ ractor drives PHASE transitions
                                 ▼  (each transition = one commit)
        surrealdb kv-lance + Rubicon timeline   ← the KANBAN SUBSTRATE
        (one commit = one version = one kanban move; #31)
```

- **Wire inside AND outside equally (then explore synergies):** treat the
  **inside** path (in-actor: direct meta-reach, hot, zero-copy SoA, O(1) pointer
  table) and the **outside** path (inter-actor: ractor/Tokio messages, detached,
  supervised, distributable) as **two co-equal, first-class transports for the
  same coordination semantics** — not a fast path with the other demoted to
  overhead. (The per-update hot path still avoids Tokio — see *Hot path* below —
  but that is path *selection* by locality, not subordination of the message
  path.) Wire both symmetrically first, then explore the synergies (deliberately left open).
- **Per mailbox (1:1:1):** each ractor mailbox owns one **BindSpace SoA** and one
  **kanban**.
- **Meta-coordination (ractor as meta, direct reach — not detached actors):** a
  **pointer table** indexes the {mailbox, SoA, kanban} triples as **O(1)**
  references, and **ractor-as-meta reaches and coordinates the mailboxes through
  it directly**. The mailboxes therefore **do not run detached** behind slow
  Tokio messages — the meta touches their SoA/kanban in-process (hot path), and
  ractor/Tokio messages are kept only for genuine cross-task async boundaries.
  *(Open: how the meta owns/reaches the triples — see §5.)*
- **Kanban substrate = ractor mailbox + surrealdb** (kv-lance + Rubicon timeline).
  **Not q2.** Each kanban card-move = one commit = one timeline version (free, per #31).
- **Rubicon phases:** **ractor owns the phase transitions in Rubicon**
  ("ractor übernimmt die Phasen"). A work item advances through phases; each
  phase transition lands as a commit on the Rubicon timeline (= a kanban move).
- **Hot path (perf) — keep the kanban OFF ractor's Tokio message path:** ractor
  delivers messages over Tokio (per-message heap box + mpsc send + task wake),
  too expensive for the per-update hot path. Because the {mailbox, SoA, kanban}
  triple is co-owned, the actor mutates the **SoA and the kanban together in one
  in-process hot-path pass** (reached O(1) via the pointer table / a shared
  lock-free snapshot) — **no extra ractor message per card-move**. Reserve
  ractor/Tokio messages for control-plane + cross-mailbox coordination; batch the
  **durable** Rubicon commit at the phase boundary. Net: the kanban has two faces
  — a **hot** in-memory projection (no Tokio cost) and a **durable** Rubicon-
  timeline projection (one batched version per phase transition).
- **Zero-copy SoA on phase transition:** a phase transition drives the
  **lance-graph SoA update** (the `witness → splat → RowDelta → apply` path).
  Because the meta reaches the SoA directly (no message-shipped payload), the
  **cognitive-shader-driver reads the very buffers lance-graph writes** — the SoA
  is **zero-copy** across the lance-graph-write → shader-read boundary (no
  marshaling/copy). The CausalEdge64 + EpisodicWitness64 rows (§2) are what the
  update touches. *(Buffer ownership/mutability contract — see §5.)*
- **Planning model (pre-planning → JIT):**
  - The actor model needs an explicit **pre-planning phase** with **wide
    expansion potential** (branch / elaborate the plan up front).
  - The collapsed **final plan runs JIT-adjacent**, either **in the mailbox** or
    as **SurrealQL → elixir-like templates**, all **inside the cognitive-shader-
    strategy orchestration**. *(Interpretation: a declarative template layer;
    the `>` and the template form need confirmation — see §5.)*
  - **lance-graph-planner → DTO:** the lance-graph planner must become a **DTO**
    wired to **ractor** and the **surrealdb kanban** — its output becomes the
    transferable plan object flowing mailbox → kanban → Rubicon timeline.

---

## 2. SoA row types: CausalEdge64 + EpisodicWitness64
- **CausalEdge64 (`ce64`)** — existing; the `lance-graph:.claude/specs/pr-ce64-mb-*`
  series is the **CausalEdge64-MailBox** line.
- **EpisodicWitness64 (`EW64`)** — **NEW**; lives **inside the SoA**. It is the
  bridge that reconnects AriGraph (see §3.B).
- **EW64 + CE64 synergies:** episodic ("what was witnessed") + causal ("which
  edges cause what") composed in the same per-mailbox SoA.

---

## 3. Work items (re-derived against the corrected architecture)

### A. BindSpace SoA — now **per-mailbox**
**Status:** SoA carrier SPEC'D; per-mailbox instancing = DESIGN.
**Plans / grounding:**
- `lance-graph:.claude/specs/pr-ce64-mb-3-bindspace-efgh.md` — BindSpace EFGH (SoA-column expansion).
- `ndarray:.claude/plans/3DGS-4x4-cognitive-shader-SoA-plan.md` — the `BindSpace4` 4-lane SoA carrier + `(4x4)^4` block fanout (the numeric substrate).
- `ndarray:.claude/prompts/05_cross_repo_map.md:88-101` — consumer migration tracked (ladybug-rs, crewai-rust reach rustynum via BindSpace).
- BindSpace today is the universal DTO in the cognitive-shader-driver (a lance-graph crate; patterns.md F).
**New constraint:** one BindSpace SoA **per mailbox**, reachable O(1) via the pointer table. Holds CausalEdge64 (existing) + EpisodicWitness64 (new) rows.

### B. AriGraph SPO is **partially disconnected** → reconnect via EpisodicWitness64
**Status:** AriGraph SPEC'D in lance-graph (shipped-status UNVERIFIED — confirm directly in the lance-graph repo); EW64 bridge = DESIGN + SPEC'D inputs.
**Problem:** AriGraph SPO is **partially disconnected** — cold facts not wired into the live path.
**Fix:** wire AriGraph into **EpisodicWitness64 inside the SoA (new)**, fed by **BOTH**:
- the **mailboxes** (hot path — live witnesses), and
- the **cold-path SPO / AriGraph facts** (stored knowledge graph).
**Plans / grounding:**
- `lance-graph:.claude/specs/pr-ce64-mb-4-arigraph-spo-g.md` — AriGraph SPO-G.
- `lance-graph:.claude/specs/pr-sprint-13-witness-cam-pq.md` — witness + CAM-PQ.
- Ingestion pattern: **`witness → splat → RowDelta → apply()`** (`lance-graph:.claude/pattern.md:236`).
- `surrealdb:.claude/board/AGENT_LOG.md:97` — "EpisodicWitness64".
**Invariant:** EW64 payload (CAM hash, SPO quad) lives **one layer up** (N1), not in kv-lance columns.

### C. ractor mailbox + surrealdb = kanban substrate
**Status:** mailbox-SoA + ractor-supervisor SPEC'D; substrate MERGED (#31); consumer = DESIGN/not built.
**Plans / grounding:**
- `lance-graph:.claude/specs/pr-ce64-mb-5-mailbox-soa-attentionmask.md` — mailbox SoA + attention mask.
- `lance-graph:.claude/specs/pr-f-1-ractor-supervisor.md` + `pr-g2-ractor-supervisor.md`; pattern F "ractor/BEAM supervisor, shape-proven" (`lance-graph:.claude/patterns.md:434,611`).
- `ractor:ractor/src/port.rs`, `ractor:docs/runtime-semantics.md` — the actor/mailbox primitive.
- `surrealdb:.claude/board/AGENT_LOG.md:96` — "ractor mailbox owns SoA → publishes onto the timeline (kanban)".
**Build (per §1):** ractor mailbox drives Rubicon phase transitions → each transition = one kv-lance commit = one kanban move = one timeline version. lance-graph-planner becomes the DTO that ractor moves across the kanban. Pre-planning phase expands; final plan runs JIT in the mailbox or as SurrealQL→elixir-like templates.

---

## 4. Cross-repo map
```
   POINTER TABLE (O(1)) ──── indexes ───┐
                                        ▼
   ractor mailbox ⟷ BindSpace SoA ⟷ kanban        [per mailbox; HOT PATH — no Tokio msg between them; CE64 + EW64 rows in the SoA]
        │ owns                ▲ holds                ▲ reconnects
        │ phase transitions   │                      │
        ▼                     │                      │
   surrealdb kv-lance + Rubicon timeline  ◀── one commit/version per move  (#31, MERGED)
        ▲                     │                      │
        │ DTO flows here      │ migrate cols         │ witnessed by
   lance-graph-planner ──DTO──┘   ndarray BindSpace4 │  AriGraph SPO  ──(hot mailbox + cold facts)──▶ EpisodicWitness64
   (becomes a DTO,                (SoA math)          (partially disconnected)        (NEW, in SoA; synergy w/ CausalEdge64)
    wired to ractor+kanban)
```
Spec home: **lance-graph/.claude/specs/pr-ce64-mb-\*** (CausalEdge64-MailBox series).
Substrate: **surrealdb kv-lance + Rubicon timeline (#31)**. SoA math: **ndarray**.
Actor/mailbox primitive: **ractor**. Planner: **lance-graph-planner → DTO**.

---

## 5. Open decisions / interpretation flags
1. **Pointer table location** — surrealdb table, in-mailbox structure, or both? What is the key (mailbox id ↔ SoA handle ↔ kanban id)?
2. **"SurrealQL → elixir-like templates"** — define this template layer; clarify what `>` denotes (compiled-to? preferred-over?) and how JIT execution chooses mailbox vs template path.
3. **Pre-planning ↔ JIT boundary** — when does the expansive pre-plan "collapse" into the JIT-adjacent final plan?
4. **lance-graph-planner DTO shape** — how it serializes, and its relation to BindSpace (is the planner-DTO carried in the BindSpace SoA, or alongside?).
5. **EW64 schema + attachment** — how an EpisodicWitness64 row attaches to a Rubicon version (one layer up, per N1) and joins hot mailbox witnesses with cold SPO/AriGraph facts.
6. **AriGraph shipped-status** — verify directly in the lance-graph repo (prior "shipped" claim was retracted).
7. **Version-pin skew** — surrealdb on lance 6.0.0/arrow 58 vs lance-graph specs on lance 6.0.1/lancedb 0.29/datafusion 53.
8. **Hot-path kanban mechanism** — exact ractor mechanism to keep kanban updates off the per-message Tokio path: same-actor in-handler mutation vs a shared `Arc`/arc-swap/lock-free snapshot resolved via the pointer table; and where the durable-commit batching boundary sits (one Rubicon version per phase transition, not per message).
9. **Meta ownership/reach model** — how ractor-as-meta owns & reaches the triples *without* detaching each mailbox as a separate message-driven Tokio task: one meta task owning all triples vs shared `Arc`/lock-free handles resolved via the pointer table — and what still legitimately needs an async message boundary.
10. **Zero-copy SoA contract** — buffer ownership + lifetime/visibility between lance-graph `apply()` (writer, on phase transition) and the cognitive-shader-driver (reader): which buffers are shared, and how to reconcile Arrow's immutable-buffer convention with a mutable per-mailbox SoA (e.g. double-buffer / arena / mutable column read each pass) so `apply` doesn't reallocate the columns the driver holds.
11. **Inside ⟷ outside synergy exploration** — wire both paths equally first, then determine which synergies are load-bearing vs incidental, and whether the inside path should be a strict *specialization* of the outside's semantics or a genuinely distinct transport. (Synergies deliberately not pre-enumerated.)
