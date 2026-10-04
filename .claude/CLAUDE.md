# .claude/ — Engineering-Session Knowledge Base

This directory is the **agent-handoff workspace** for AdaWorldAPI's
SurrealDB fork. It contains scaffolds, design docs, and prompts that
let a fresh Claude/Claude-Code session resume work without losing
context.

> The upstream SurrealDB `CLAUDE.md` (at repo root) is **untouched**
> and remains authoritative for SurrealDB-internal conventions.
> This `.claude/` directory only carries AdaWorldAPI-specific work
> that sits on top of upstream.

## What's here

```
.claude/
├── CLAUDE.md                       ← you are here (orientation)
├── BOOT.md                         ← startup ritual for new sessions
├── settings.json                   ← workspace permissions + hook wiring
├── board/                          ← APPEND-ONLY session ledgers
│   ├── AGENT_LOG.md                ← per-session run log (newest first)
│   └── EPIPHANIES.md               ← findings / conjectures (newest first)
├── knowledge/                      ← reference docs (READ BY: …)
│   ├── lance-api-surface.md        ← Lance Dataset API the TODOs call
│   └── transactable-contract.md    ← the 19 trait methods + invariants
├── hooks/
│   └── session-start.sh            ← injects read order at turn 0
└── lance-backend/                  ← current active project
    ├── README.md                   ← architecture, design, roadmap
    ├── DAY_BY_DAY.md               ← 12-day implementation plan
    ├── lance/                      ← scaffold code (to be copied
    │                                  into surrealdb/core/src/kvs/)
    │   ├── mod.rs                  ← Datastore + Transaction + Transactable
    │   ├── schema.rs               ← Arrow KV schema + predicate builders
    │   ├── tx_buffer.rs            ← Pending-writes buffer
    │   ├── cnf.rs                  ← SURREAL_LANCE_* config
    │   └── background_optimizer.rs ← Periodic Dataset::optimize() task
    └── patches/                    ← patches to existing surrealdb files
        ├── kvs-mod.patch.rs        ← register lance module in kvs/mod.rs
        ├── kvs-config.patch.rs     ← add LanceConfig to config.rs
        ├── kvs-ds.patch.rs         ← DatastoreFlavor::Lance + URL handler
        └── Cargo-toml.patch.txt    ← kv-lance feature + deps
```

## Active project: Lance backend (`kv-lance`)

A storage backend for SurrealDB built on top of the Lance versioned
columnar format. See `lance-backend/README.md` for the full
architecture story; see `lance-backend/DAY_BY_DAY.md` for the
concrete implementation plan.

**Status:** scaffold complete (all 19 `Transactable` trait methods
stubbed with structurally-compiling Rust). The Lance API call sites
are marked `TODO(lance-integration)` and the integration is the
12-day work item.

## Working agreement (for agents)

This workspace mirrors the AdaWorldAPI pattern used in WoA, lance-graph,
and other repos. Four rules:

1. **Read BOOT.md first.** Every session. No exceptions. It's the
   loading ritual — orient yourself before touching anything.
2. **Append don't overwrite.** Two flavours:
   - **Project files.** When adding new files here, place them in
     a project subdirectory. Don't modify upstream files (e.g.
     `surrealdb/core/...`) without an explicit patch file in the
     relevant project's `patches/` directory.
   - **Board files.** `board/AGENT_LOG.md` and `board/EPIPHANIES.md`
     are append-only ledgers — write only via `tee -a`, never via
     `Edit` / `Write` / `>` redirection. Old entries never mutate;
     corrections append as dated entries citing the original.
3. **Knowledge before code.** Before resolving a `TODO(lance-integration)`
   site, load the matching `knowledge/*.md` doc:
   - `knowledge/lance-api-surface.md` — for any TODO that calls
     into `lance::Dataset` / `Transaction`.
   - `knowledge/transactable-contract.md` — for any change to a
     `Transactable` trait method.
4. **Commit small.** Each meaningful work step is one commit with a
   clear message. Mirror Stefan's pattern from WoA. Append one
   `AGENT_LOG.md` entry per commit batch.

## What this directory is NOT

- Not a substitute for the upstream `CLAUDE.md` at repo root
- Not a place to store secrets or credentials
- Not a runtime configuration directory (the actual code lives in
  `surrealdb/core/src/kvs/lance/` once patches are applied)

## Conventions

- **Patches** end with `.patch.rs` or `.patch.txt` and contain a
  comment block at the top describing exactly where to apply them.
- **Scaffolds** are full files, ready to be copied into their target
  location.
- **READMEs** stay current with the actual state of the work.
- **DAY_BY_DAY.md** files have checkbox lists; agents tick them as
  work progresses.
