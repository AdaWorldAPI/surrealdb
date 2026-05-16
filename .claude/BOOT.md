# BOOT.md — Session Startup Ritual

Read this file **first** on every new session. It's a 5-minute orient.

## 1. Read these files, in order

1. **The upstream `CLAUDE.md`** at the repo root.
   - This is SurrealDB's own agent-guidance, written by the SurrealDB
     maintainers. Authoritative for SurrealDB-internal conventions.
   - Build commands, test patterns, code style, etc. live there.
2. **This directory's `CLAUDE.md`** (next to this file).
   - Explains what `.claude/` is for and what the AdaWorldAPI work
     adds on top of upstream.
3. **`.claude/board/AGENT_LOG.md`**.
   - APPEND-ONLY (newest first). Tells you what prior sessions did,
     what was committed, what's still open. Skim the top 3 entries.
4. **`.claude/board/EPIPHANIES.md`**.
   - APPEND-ONLY (newest first). Findings and conjectures that
     affect how to interpret the scaffold. Skim entries marked
     `**Status:** FINDING` for invariants you must preserve.
5. **The active project's `README.md`** (e.g.
   `lance-backend/README.md`).
   - Tells you what the project is, what's done, what's pending.
6. **The active project's `DAY_BY_DAY.md`**.
   - Tells you what to do next. Find the first unchecked box.

If the unchecked box involves a `TODO(lance-integration)` site,
also load the relevant knowledge doc:

- `.claude/knowledge/lance-api-surface.md` — when the TODO calls
  `lance::Dataset` / `lance::Transaction` methods.
- `.claude/knowledge/transactable-contract.md` — when the TODO is
  in a `Transactable` trait method body.

## 2. Understand the working context

- **Repo origin:** fork of `surrealdb/surrealdb`, owned by AdaWorldAPI.
- **License:** BSL 1.1 → Apache 2.0 in 2030. Internal use is fine,
  reselling as DBaaS is not. Most AdaWorldAPI use cases qualify as
  internal.
- **Companion repos** in the same ecosystem:
  - `AdaWorldAPI/lance-graph` — Cypher + GraphBLAS engine on Lance
  - `AdaWorldAPI/ndarray` — HPC fork with SIMD kernels, 611M
    similarity comparisons/sec on consumer CPU
  - `AdaWorldAPI/WoA` — Stefan's production app (Python, work order
    management, separate project but shares `.claude/` conventions)

## 3. Iron rules

These come from Stefan's WoA project but apply equally here:

1. **Read the actual source before describing it.** Memory notes and
   prior session output can be stale. Verify against the live file.
2. **Don't break upstream patterns.** SurrealDB has conventions
   (error handling, async patterns, instrument macros, naming) that
   the maintainers care about. Mirror them.
3. **Patches stay isolated.** Don't modify upstream files in place;
   create a `.patch.rs` / `.patch.txt` file under the relevant
   project's `patches/` directory.
4. **Small atomic commits.** One logical change per commit. Stefan's
   pattern: "Step X.Y: <what was done>".
5. **Tests before claims.** If you say "this works", run the test
   that proves it. If the test doesn't exist, write it.
6. **No silent shortcuts.** If you skip a planned step, document why
   in the commit message or in `DAY_BY_DAY.md`.
7. **Cite the spec.** When implementing a `Transactable` method, the
   spec is the trait doc-comment in `surrealdb/core/src/kvs/api.rs`.
   When in doubt, that's authoritative.
8. **Stop and ask.** If a design decision will affect downstream
   work (e.g. schema change, transaction-semantic difference),
   surface it explicitly before proceeding.
9. **Preflight the full canonical battery before every commit.**
   CI is the second line of defense; preflight is the first.

   **Canonical (every PR):**
   - `cargo clippy -p <crate> --features "<set>" -- -D warnings`
     Strict tier (when pedantic-clean is the goal):
     `-- -D warnings -D clippy::pedantic -D clippy::nursery`
     ~600 lints. Floor, not goal — fix at the source, don't
     `#[allow]` past unless rationale is documented inline.
   - `cargo fmt --check`
     Rustfmt 1.95 (matches `rust-toolchain.toml`). Has hit every
     sprint-11/12 PR's CI; preflighting it is non-negotiable.
   - `cargo audit`
     RustSec advisory scan.
   - `cargo deny check`
     License + dep + advisory + bans. Closest single-binary
     "ruff-ish" multi-axis check.

   **Quality / maintenance (per sprint or per substantial PR):**
   - `cargo machete` — unused-dep detector.
   - `cargo geiger` — unsafe scan. Every `unsafe` block needs
     a `// SAFETY:` comment (upstream CLAUDE.md rule).
   - `cargo semver-checks check-release` — public-API SemVer
     compat (catches accidental breakage on shipped surfaces).
   - `cargo spellcheck` — comments + docs.
   - `cargo public-api` — surface diff (paired with semver-checks
     for the "did I just add a public item?" check).

   **Heavier / opt-in (all stable Rust):**
   - `kani` — bounded model checker, `#[kani::proof]` harnesses
     for invariant proofs.
   - `loom` — concurrency model checker (lib, not CLI; wire into
     `#[cfg(loom)]` tests).
   - `cargo mutants` — mutation testing to validate test coverage
     actually catches breakages.
   - `cargo-tarpaulin` — coverage.

   Iron rule applies to the **canonical** tier on every PR. Quality
   tier should run before merging substantial changes. Heavy tier
   is sprint-level or release-gate.

## 4. What to do first

If you're a fresh session with no specific instruction:

1. Look at `lance-backend/DAY_BY_DAY.md`.
2. Find the first unchecked box.
3. Read the surrounding context (often a `TODO(lance-integration)`
   block in the relevant `lance/*.rs` file). Load the matching
   knowledge doc (`knowledge/lance-api-surface.md` or
   `knowledge/transactable-contract.md`).
4. Do that one task.
5. Check the box. Commit. Append one entry to
   `.claude/board/AGENT_LOG.md` via `tee -a`. Stop.

If you have a specific instruction from a human, do that instead —
but still read the files in step 1 first.

## 5. When something feels wrong

- If the upstream `CLAUDE.md` and this directory's `CLAUDE.md`
  conflict: upstream wins for upstream code, ours wins for `.claude/`.
- If a `TODO(lance-integration)` comment references a Lance API
  that doesn't exist in the current crate version: check
  `lance-graph/Cargo.toml` for the pinned version, then `cargo doc
  --open --package lance` to see the actual API surface.
- If the test you wrote passes but the SurrealDB integration test
  suite fails: that's a semantic mismatch. Document it in
  `lance-backend/KNOWN_DIFFERENCES.md` and bring it up with the
  human.

---

**Time spent on this ritual: ~5 minutes.**
**Time saved over a session: hours of rediscovering decisions.**
