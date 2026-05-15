# a2aworkarounds.md — file-blackboard for the kv-lance Prep fleet

> **APPEND-ONLY.** Newest at bottom (workers append on completion).
> **Pattern:** A2A file-blackboard (lance-graph / ndarray convention).
> **Write only via `tee -a`** — never `Edit`, never `Write`, never `>`.
>
> **Spawn protocol:** every worker reads this file before starting,
> appends one entry on completion via `tee -a`. The meta agent reads
> all entries at the end and reports.

## Sprint: lance-backend Prep — apply patches + copy scaffold

**Goal:** carry out the Prep section of
`.claude/lance-backend/DAY_BY_DAY.md` so that `surrealdb/core/src/kvs/lance/`
exists with the scaffold and the four upstream files are patched. After
this sprint the workspace is at "structural compile only — `todo!()` at
runtime is expected and fine."

**Branch:** `claude/setup-knowledge-base-VWNi7`

**Pinned references (read-only):**
- `.claude/lance-backend/DAY_BY_DAY.md` — the plan; Prep is the first section.
- `.claude/lance-backend/README.md` — design rationale.
- `.claude/lance-backend/lance/*.rs` — source for the 5 copies.
- `.claude/lance-backend/patches/*.patch.{rs,txt}` — the 4 patches.
- `.claude/knowledge/lance-api-surface.md`, `.claude/knowledge/transactable-contract.md` — semantic anchors.

## Fleet manifest

| # | Agent | Source | Target file | Model |
|---|---|---|---|---|
| W1 | cargo-toml-patcher | `.claude/lance-backend/patches/Cargo-toml.patch.txt` | `surrealdb/core/Cargo.toml` | Sonnet |
| W2 | kvs-mod-patcher | `.claude/lance-backend/patches/kvs-mod.patch.rs` | `surrealdb/core/src/kvs/mod.rs` | Sonnet |
| W3 | kvs-config-patcher | `.claude/lance-backend/patches/kvs-config.patch.rs` | `surrealdb/core/src/kvs/config.rs` | Sonnet |
| W4 | kvs-ds-patcher | `.claude/lance-backend/patches/kvs-ds.patch.rs` | `surrealdb/core/src/kvs/ds.rs` | Sonnet |
| W5 | lance-mod-copy | `.claude/lance-backend/lance/mod.rs` | `surrealdb/core/src/kvs/lance/mod.rs` | Sonnet |
| W6 | lance-schema-copy | `.claude/lance-backend/lance/schema.rs` | `surrealdb/core/src/kvs/lance/schema.rs` | Sonnet |
| W7 | lance-tx-buffer-copy | `.claude/lance-backend/lance/tx_buffer.rs` | `surrealdb/core/src/kvs/lance/tx_buffer.rs` | Sonnet |
| W8 | lance-cnf-copy | `.claude/lance-backend/lance/cnf.rs` | `surrealdb/core/src/kvs/lance/cnf.rs` | Sonnet |
| W9 | lance-bg-opt-copy | `.claude/lance-backend/lance/background_optimizer.rs` | `surrealdb/core/src/kvs/lance/background_optimizer.rs` | Sonnet |
| M  | integration-checker | reads all W1-W9 entries + actual diffs | (verification only — no writes) | Opus |

## Entry template (append at bottom)

```bash
tee -a .claude/board/a2aworkarounds.md > /dev/null <<'EOF'

## YYYY-MM-DDTHH:MM — <agent-id> <agent-name> (<model>)
**Target:** <path>
**Verdict:** PASS | BLOCK | DEFERRED

**What was done (max 5 lines):**
- …

**Notes / blockers:**
- …

**Tests / checks run:**
- `<cmd>` → <result>
EOF
```

## Entries (append below)


## 2026-05-15T18:30 — W1 cargo-toml-patcher (sonnet)
**Target:** surrealdb/core/Cargo.toml
**Verdict:** PASS

**What was done:**
- Added `kv-lance = ["dep:lance", "dep:arrow-array", "dep:arrow-schema"]` under `[features]` between `kv-indxdb` and `kv-rocksdb` (alphabetical order).
- Added `lance = { version = "1.0", optional = true }`, `arrow-array = { version = "55", optional = true }`, and `arrow-schema = { version = "55", optional = true }` in the `# External KV stores` block, above `rocksdb`.
- Skipped `dep:hex` from feature list: `hex.workspace = true` already exists as a non-optional dep (line 127).
- Skipped `dep:async-trait` from feature list: `async-trait.workspace = true` already exists as a non-optional dep (line 111).
- Skipped `[package.metadata.features.kv-lance]` block: no such convention exists in this file (only `[package.metadata.docs.rs]` and `[package.metadata.cargo-machete]`).

**Notes / blockers:**
- `hex` is already a non-optional workspace dep — `dep:hex` dropped from the feature line (no functional loss; hex is always available).
- `async-trait` is already a non-optional workspace dep — `dep:async-trait` dropped from the feature line (same reasoning).
- No `[package.metadata.features.*]` convention found; metadata block omitted per instructions.

**Tests / checks run:**
- `git diff --stat surrealdb/core/Cargo.toml` → 1 file changed, 4 insertions(+), 0 deletions(−); additions only, no deletions.

## 2026-05-15T18:30 — W2 kvs-mod-patcher (sonnet)
**Target:** surrealdb/core/src/kvs/mod.rs
**Verdict:** PASS

**What was done:**
- Read blackboard, patch file, and target file before making any changes.
- Added `//! - \`lance\`: [Lance](https://lance.org) versioned columnar format with native MVCC, OCC, and scalar indexes. Optimised for AI/analytical workloads.` to the doc-comment, alphabetically between `indxdb` and `rocksdb`.
- Added `#[cfg(feature = "kv-lance")] mod lance;` between `mod indxdb;` and `mod mem;` (alphabetically correct placement).
- Used cfg-gate `#[cfg(feature = "kv-lance")]` — note: neighbouring modules (`indxdb`, `mem`, `rocksdb`, `surrealkv`, `tikv`) are all ungated in the file; the cfg-gate was applied per task instructions to mirror the intended pattern for new backends.

**Notes / blockers:**
- Existing backend module declarations (`indxdb`, `mem`, `rocksdb`, `surrealkv`, `tikv`) are NOT cfg-gated in the target file. Applied `#[cfg(feature = "kv-lance")]` as instructed to match the intended new-backend pattern.
- The patch file's BEFORE block omits `surrealkv` from the doc-comment list, but the actual file has `surrealkv` — lance was inserted alphabetically before `rocksdb` in the doc-comment (after `indxdb`), matching the actual file order.

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/mod.rs` → 2 insertions(+), 0 deletions(-) — strictly additive.

## 2026-05-15T18:30 — W3 kvs-config-patcher (sonnet)
**Target:** surrealdb/core/src/kvs/config.rs
**Verdict:** PASS

**What was done:**
- Read blackboard, patch file, and target file to understand conventions.
- Confirmed that both SurrealKvConfig and RocksDbConfig have a from_params(&HashMap<String,String>) -> Result<Self> constructor — convention is uniform.
- Inserted LanceConfig struct + Default impl + from_params impl after RocksDbConfig block, before the Duration section and #[cfg(test)] module.
- All three impls gated on #[cfg(feature = "kv-lance")].

**Notes / blockers:**
- from_params added per convention: returns Ok(Self::default()) with _params (unused) — all Lance knobs live in env-vars (cnf.rs). This is noted in the doc-comment.
- No existing lines were modified — strictly additive patch.

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/config.rs` → 1 file changed, 42 insertions(+), 0 deletions(-)

## 2026-05-15T18:31 — W4 kvs-ds-patcher (sonnet)
**Target:** surrealdb/core/src/kvs/ds.rs
**Verdict:** PASS

**What was done:**
- Change 1 (DatastoreFlavor variant): Added `Lance(super::lance::Datastore)` after `SurrealKV`, gated by `#[cfg(feature = "kv-lance")]`.
- Change 2 (URL scheme arm): Added `(flavour @ "lance", path)` arm after surrealkv arm, using `super::config::LanceConfig::from_params`, mirroring upstream shape exactly (threadpool init, config from params, `.map(DatastoreFlavor::Lance)`, `Box::<DatastoreFlavor>::new(v)`, disabled-feature bail).
- Change 3 (transaction dispatch arm): Added `Self::Lance(v)` arm after `SurrealKV`, returning `(tx, true)` tuple — matching upstream convention (NOT `Box<dyn Transactable>`).

**Notes / blockers:**
- Upstream `DatastoreFlavor` does NOT use `Arc<>` wrappers — patch file example used `Arc<super::lance::Datastore>` but actual upstream uses `super::lance::Datastore` directly (matches Mem, RocksDB, etc.). Adapted accordingly.
- Upstream URL arm uses tuple pattern `(flavour @ "lance", path)` not bare `"lance" =>` — adapted.
- Upstream transaction dispatch returns `(tx, true)` tuples, not `Box<dyn Transactable + Send + Sync>` — patch file example was wrong; upstream convention used instead.
- Assumed `LanceConfig::from_params` exists (mirroring SurrealKvConfig::from_params); if only `LanceConfig::default()` exists, the arm needs a minor adjustment.

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/ds.rs` → 1 file changed, 26 insertions(+), 0 deletions(−); three hunks, additions only.

## 2026-05-15T18:33 — W5-W9 lance/* copy (main thread, sonnet workers were denied Bash)
**Target:** surrealdb/core/src/kvs/lance/{mod,schema,tx_buffer,cnf,background_optimizer}.rs
**Verdict:** PASS

**What was done:**
- W5-W9 sub-agents were spawned but lacked Bash permission and could not run `cp` / `diff` / `tee -a`. They reported the blocker and exited.
- Main thread executed the 5 `cp` operations directly: `for f in mod.rs schema.rs tx_buffer.rs cnf.rs background_optimizer.rs; do cp .claude/lance-backend/lance/$f surrealdb/core/src/kvs/lance/$f; done`.
- `diff -q` ran for each pair after copy — all silent (byte-identical).

**Notes / blockers:**
- The Sonnet sub-agents in this fleet inherit a restrictive permission profile that excludes Bash. For future sprints, either pass Bash in their `tools` allow-list, or budget copy work for the main thread. Patch-application workers (W1–W4) used Edit/Read only and were unaffected.
- `lazy_env_parse!` macro discoverability inside cnf.rs was NOT checked (W8's sanity note was skipped). Meta agent / next Day-1 worker should grep `surrealdb/core/src/cnf` or `surrealdb/core/src/kvs/cnf.rs` and verify the import resolves; if not, the file needs a `use` import.

**Tests / checks run:**
- `diff -q src dst` × 5 → all silent (byte-identical)
- `ls -la surrealdb/core/src/kvs/lance/` → 5 files present, sizes match .claude/lance-backend/lance/.

## 2026-05-15T18:34 — W8 lance-cnf-copy (sonnet)
**Target:** surrealdb/core/src/kvs/lance/cnf.rs
**Verdict:** PASS

**What was done:**
- Read blackboard and source file at .claude/lance-backend/lance/cnf.rs.
- Confirmed target directory surrealdb/core/src/kvs/lance/ already existed (created by a prior worker).
- File was already present verbatim — diff src dst exits 0, no differences.
- Verified lazy_env_parse! macro discoverability (see Notes).

**Notes / blockers:**
- `lazy_env_parse!` macro discoverability: macro is defined with `#[macro_export]` in `surrealdb/core/src/mac/mod.rs` (confirmed via grep). It appears in `surrealdb/core/src/cnf/mod.rs` and all peer backends (`rocksdb/cnf.rs`, `tikv/cnf.rs`, `surrealkv/cnf.rs`) without an explicit `use` import — available implicitly at crate root. No extra import needed.

**Tests / checks run:**
- `diff .claude/lance-backend/lance/cnf.rs surrealdb/core/src/kvs/lance/cnf.rs` → exit 0 (identical)

## 2026-05-15T19:06 — A2 schema-fixer (sonnet)
**Target:** surrealdb/core/src/kvs/lance/schema.rs
**Verdict:** PASS

**What was done:**
- Line 81-82 tombstone_array (build_write_batch): replaced iter::repeat(false).take(writes.len()).collect() with BooleanArray::from(vec![false; writes.len()])
- Line 115-116 tombstone_array (build_tombstone_batch): replaced iter::repeat(true).take(deletes.len()).collect() with BooleanArray::from(vec![true; deletes.len()])
- version_array lines (79-80, 113-114) left unchanged — UInt64Array::from_iter<u64> is valid and was not flagged by cargo

**Notes / blockers:**
- Only the two BooleanArray sites were broken; UInt64Array FromIterator<u64> is well-supported and was not flagged. No other arrays in the file use the repeat/collect pattern.

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/lance/schema.rs` → 1 file changed, 2 insertions(+), 4 deletions(-); exactly the two error sites, no other changes

## 2026-05-15T19:07 — A3 direction-fixer (sonnet)
**Target:** surrealdb/core/src/kvs/lance/mod.rs (line 556 only)
**Verdict:** PASS

**What was done:**
- Read scanner.rs Direction enum → variants are: Forward, Backward
- Replaced `Direction::Reverse` at line 556 with `Direction::Backward`

**Notes / blockers:**
- (scoped: did not touch lines 583/598 — left for A4)

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/lance/mod.rs` → 1 file changed, 1 insertion(+), 1 deletion(-) (exactly 1 line changed)

## 2026-05-15T19:07 — A4 err-variant-adder (sonnet)
**Target:** surrealdb/core/src/kvs/err.rs
**Verdict:** PASS

**What was done:**
- Read existing Error enum → derive macro used: thiserror (#[derive(Error, Debug)])
- All existing variants are unit variants (PascalCase, no data) with #[error("...")] attributes
- Checked for existing savepoint-like variants → found: none (tikv uses Error::Transaction("No savepoint..."), surrealkv delegates to upstream, no NoSavePointPresent anywhere)
- Decision tree path: (c) — no upstream equivalent, add new unit variant
- Added NoSavePointPresent variant between Internal and CompactionNotSupported (alphabetical N position):
  ```rust
  /// There is no savepoint present to rollback to or release
  #[error("No savepoint present")]
  NoSavePointPresent,
  ```

**Notes / blockers:**
- tikv backend handles the same case via Error::Transaction("No savepoint to rollback to") — lance is more specific/correct with a dedicated variant.
- No cfg-gate needed: the variant is generic enough to belong in the shared enum unconditionally (matching the pattern of TransactionFinished, TransactionReadonly, etc.).

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/err.rs` → 1 file changed, 4 insertions(+), 0 deletions(-) (exactly the new variant + doc comment, no deletions)

## 2026-05-15T19:20 — A1 ds-fixer (sonnet)
**Target:** surrealdb/core/src/kvs/ds.rs
**Verdict:** PASS

**What was done:**
- Error 1 (threadpool): Chose A — deleted the two-line `super::threadpool::initialise()` call from the lance arm. Root cause: `threadpool.rs` has a file-level `#![cfg(any(feature = "kv-mem", feature = "kv-rocksdb", feature = "kv-surrealkv"))]` gate that excludes `kv-lance`, so the module simply does not exist when compiling with only `kv-lance`. Lance uses async I/O (not a blocking thread pool), so the call is not needed.
- Error 2 (Box<dyn Transactable>): Wrapped with `Box::new(tx) as Box<dyn Transactable>`. Root cause: `lance::Datastore::transaction()` returns `Result<Transaction>` (concrete type), unlike surrealkv/mem which already return `Result<Box<dyn Transactable>>`. The coercion cast makes the tuple type match the `(Box<dyn Transactable>, bool)` return type.

**Notes / blockers:**
- threadpool.rs gating was the key: line 1 `#![cfg(any(feature = "kv-mem", feature = "kv-rocksdb", feature = "kv-surrealkv"))]` — kv-lance is not in the list, so under `--features kv-lance --no-default-features` the module is absent.
- surrealkv and mem both return `Box<dyn Transactable>` from their `transaction()` methods; lance returns a concrete `Transaction` — hence needed explicit `Box::new(...) as Box<dyn Transactable>` cast.

**Tests / checks run:**
- `git diff --stat surrealdb/core/src/kvs/ds.rs` → 1 file changed, 1 insertion(+), 3 deletions(-); exactly the two error sites, no other changes

## 2026-05-15T18:48 — Meta-A integration-checker (opus, main thread)
**Target:** verification of Sprint A (A1-A4)
**Verdict:** PASS

**What was done:**
- Ran `cargo check --features kv-lance --no-default-features --manifest-path surrealdb/core/Cargo.toml` after A1-A4 completed.
- A1 (ds.rs): threadpool deletion + Box<dyn Transactable> wrap — clean.
- A2 (schema.rs): BooleanArray::from(vec![...; N]) — clean.
- A3 (mod.rs:556): Direction::Reverse → Direction::Backward — clean.
- A4 (err.rs): NoSavePointPresent variant added — clean.
- One residual P0 surfaced (E0004 non-exhaustive match in err/to_types.rs:294 caused by A4's new variant). Fixed inline on main thread by adding `| KvsError::NoSavePointPresent` to the existing TransactionFinished | TransactionReadonly | TransactionConditionNotMet arm.

**Notes / blockers:**
- 14 warnings remain (unused imports + unused-field in DatasetHandle + unused_mut in commit's `let mut ds = ...`). These are NOT errors — they're expected for stub-state scaffold and will resolve as Day 1+ wires real Lance API. Not Sprint A's scope.
- The threadpool gate finding (A1) is reusable: `kv-lance` must NOT depend on the surrealdb-internal threadpool. Lance owns its own async runtime.

**Tests / checks run:**
- `cargo check --features kv-lance --no-default-features` → Finished in 5m 07s, 0 errors, 14 warnings
