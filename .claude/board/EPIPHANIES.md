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
