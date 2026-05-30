# RESEARCH_LOG — Unified WAL/ACID/SoA investigation (append-only, tee -a)

> GOAL: design a UNIFIED WAL so SurrealDB (surrealkv) and lance-graph (Sosumi)
> write/read the SAME SoA store with NO duplication. Bottom-up arc:
> unified WAL -> DAIS (zero-copy SoA access layer) -> cognitive shader.
> RESEARCH ONLY — no implementation code this round.
>
> Sources: /home/user/surrealdb (surreal + kv-lance), /home/user/lance-graph
> (Sosumi), surrealkv-0.21.2 (cargo cache), lance 6.0.0. gemini/codex CLIs
> NOT installed -> no external GPT/Gemini confrontation; Claude-only analysis.
>
> Key questions to confirm/challenge:
>  Q1 surreal txn = atomic over MANY keys; lance write = atomic per batch/commit. Reconcile.
>  Q2 can lance-graph write & surreal read (SoA or KV) and back — one store?
>  Q3 is surrealkv's WAL the same shape as lance's commit-log, or different?

## Log
[ORCH] START 2026-05-30T12:02:08Z — branch claude/unified-wal-research; lance-graph cloned; surrealkv-0.21.2 located; Sosumi dispatched (lance-graph), self on surrealkv+lance.
[SOSUMI] START lance-graph write-path investigation (2026-05-30) — read-only mode.
[SOSUMI] BLOCKED: tool-result output channel returned empty for all Bash and Read calls after the first turn. Cannot read source to produce code-cited findings. Aborting rather than fabricate citations. Repo confirmed present at /home/user/lance-graph (crates/, docs/, .claude/ all exist). Named docs data-flow.md / COMMIT_GATE_OPTIMIZATION.md / ndsoa.md / fault-tolerant.md / LANCEGRAPH_INTEGRATION_PLAN.md were NOT found in top-level docs/ (find returned nothing visible; likely absent or under different names).

[ORCH] CORRECTION 2026-05-30 — FAITHFULNESS BREACH (caught by user). I began modeling the
unified WAL on **surrealkv's** WAL/oracle. surrealkv is a SEPARATE engine we do NOT use; the
stack rides **lance6 / lancedb**. RE-ORIENT:
  - The unified WAL IS lance/lancedb's native atomic versioned commit (manifest chain = the
    log = the ACID boundary = the MVCC axis). Nothing to import, nothing to reinvent.
  - surreal does NOT bring its WAL. It rides lance's. The ONLY thing taken from the surreal
    side is the SEMANTIC contract: a SurrealDB multi-key transaction == ONE lance commit
    (one batch / one new version / one commit timestamp). [confirmed: surrealkv transaction.rs
    commit() collects write_set into one Batch under one commit_timestamp — the semantic we
    must preserve over lance, NOT the mechanism we copy.]
  - Q3 answered: "surrealkv wal == lancedb wal" is the wrong axis. lance's commit IS the WAL.
  - DROP the surrealkv-as-foundation line. STUDY lance6/lancedb native commit/version/OCC
    internals as the substrate. Open faithfulness check: does the kv-lance front-WAL/memtable
    even need to exist, or do lance-native batched commits suffice?
  - Sosumi (lance-graph = the faithful lance consumer) continues unchanged.
[ORCH] FINDING 2026-05-30T12:08:32Z — lance6 native transaction→manifest commit IS the unified WAL (transaction.rs:4-67, OCC retry, 1:1 txn↔version). surreal multi-key txn == one lance txn (semantic only). kv-lance bespoke on-disk WAL = retire; keep in-mem Arrow batch→one lance commit. Wrote unified-wal-faithful.md. Awaiting Sosumi for lance-graph commit cadence.
