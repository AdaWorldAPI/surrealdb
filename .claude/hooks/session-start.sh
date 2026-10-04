#!/usr/bin/env bash
# SessionStart hook — inject the surrealdb `.claude/` read order at turn 0.
# Emits JSON on stdout; Claude consumes hookSpecificOutput.additionalContext
# as a system reminder in the first model turn.
#
# Wired from: .claude/settings.json
set -euo pipefail

cat <<'JSON'
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "WORKSPACE BOOTLOAD (AdaWorldAPI/surrealdb):\n\n## Mandatory reads this session (in order)\n\n1. CLAUDE.md at the repo root — upstream SurrealDB's own agent guidance (build commands, test patterns, code style).\n2. .claude/CLAUDE.md — what `.claude/` is, the directory tree, the working agreement.\n3. .claude/BOOT.md — startup ritual, iron rules, what-to-do-first.\n4. .claude/board/AGENT_LOG.md — what prior sessions did. APPEND-ONLY (newest at top).\n5. .claude/board/EPIPHANIES.md — findings/conjectures. APPEND-ONLY.\n6. .claude/lance-backend/README.md and DAY_BY_DAY.md — the active project. Find the first unchecked box.\n\n## Knowledge docs to load when scope matches\n\n- .claude/knowledge/lance-api-surface.md — load before resolving any TODO(lance-integration).\n- .claude/knowledge/transactable-contract.md — load before implementing or modifying any `Transactable` method.\n\n## Append-only discipline (governance)\n\nBoard files (`.claude/board/AGENT_LOG.md`, `.claude/board/EPIPHANIES.md`) are written ONLY via `tee -a`. Never `Edit`, never `Write`, never `>` redirection. Old entries never mutate — corrections append as dated entries citing the original.\n\nEntry template lives in each file's header.\n\n## Iron rules (summary; full text in .claude/BOOT.md §3)\n\n- Read actual source before describing it. Memory and prior session output can be stale.\n- Don't break upstream patterns. SurrealDB conventions live in the root CLAUDE.md.\n- Patches stay isolated. Modify upstream files only via .claude/lance-backend/patches/*.patch.{rs,txt}.\n- Small atomic commits. One logical change per commit. Append to AGENT_LOG.md on completion.\n- Tests before claims. If 'this works', run the test that proves it.\n- Stop and ask. If a design decision affects downstream work, surface it before proceeding."
  }
}
JSON
