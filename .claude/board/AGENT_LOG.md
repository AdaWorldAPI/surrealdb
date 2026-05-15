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
