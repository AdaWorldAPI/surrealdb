# GRIDLAKE — Aligning SurrealDB Writes With Lance

> **The whole shebang, documented.** How the `kv-lance` backend gives
> SurrealDB **RocksDB-grade WAL/ACID** and **ClickHouse-grade write
> batching** while staying faithful to Lance's columnar/versioned model.
>
> **Status of this doc:** design + as-built audit. Every architectural
> claim is cited to the actual code in `surrealdb/core/src/kvs/lance/**`
> and `surrealdb/core/src/kvs/{config,mvcc_source}.rs`. A sibling CODE
> agent is editing those files concurrently, so citations are by
> **symbol/concept**; line numbers are **approximate** (`~L`) and may
> drift by a few lines.
>
> **Conjecture markers.** Established (read in-tree) facts are stated
> plainly. Forward-looking proposals carry an explicit
> **(CONJECTURE)** / **(ROADMAP)** tag. The two must never be confused.

---

## 0. Reading map

| Section | What it answers |
| ------- | --------------- |
| [1](#1-the-convergent-ingestmigratecompact-pattern) | Why RocksDB, ClickHouse, SurrealKV and Lance are the *same* machine, and the 5 invariants they share |
| [2](#2-mapping-the-pattern-onto-kv-lance) | Which real symbol in `kvs/lance/**` plays which role |
| [3](#3-wal--acid-story) | Exactly where A, C, I, D are each enforced |
| [4](#4-clickhouse-parity) | parts↔fragments, async_insert↔flusher, the "don't out-run merges" discipline |
| [5](#5-the-commit-sequence-seqno-keystone) | Why `version` is flush-granular, why a per-row `seq` is the fix |
| [6](#6-the-soa--gridlake-alignment) | Killing the per-flush row→column transpose |
| [7](#7-compaction-gc) | Reclaiming tombstone rows via deletion-vector compaction |
| [8](#8-phased-roadmap) | What is IMPLEMENTED this session vs ROADMAP |
| [9](#9-faithfulness-constraints) | Stable Rust, stable Lance contract, no new deps, CI reality |

**Sibling docs:** `README.md` (backend overview), `ARCHITECTURE_VISION.md`,
`KNOWN_DIFFERENCES.md`, `BENCH_RESULTS.md`, `UPSTREAM_TEST_RESULTS.md`.
**Findings ledger:** `.claude/board/EPIPHANIES.md` (the codex P1 fix and
the CI/rustfmt split-brain are the two load-bearing recent entries).

---

## 1. The convergent ingest→migrate→compact pattern

Every production write-optimised store, no matter how its marketing
positions it, is built around the same three-phase pipeline:

```text
   INGEST              MIGRATE                 COMPACT
   (row-sequential,    (deferred, batched,     (background GC,
    durable, fast)      columnar/immutable)     reclaim + reshape)

   RocksDB:    WAL  →  memtable→SST flush   →   leveled compaction
   ClickHouse: (async)→ part write          →   MergeTree merges
   SurrealKV:  WAL  →  LSM segment build    →   segment compaction
   Lance(here):WAL  →  memtable→merge_insert →  Dataset::optimize
```

The names differ; the **machine is identical**. Five invariants fall out
of this convergence, and `kv-lance` obeys all five.

### The 5 invariants

1. **Durability is row-sequential and group-committed; the columnar
   write is deferred and batched.** You make a write durable by
   appending it to a sequential log and fsyncing (cheap, O(1) seeks),
   *not* by writing the columnar/immutable structure inline (expensive,
   rewrites a manifest/SST). The columnar structure is built later, in
   bulk, amortising the per-row cost. — *In `kv-lance`:* the WAL append +
   fsync is the durability point; the Lance `merge_insert` is the
   deferred columnar write (`flusher.rs::do_flush`).

2. **Tiny commits are the enemy.** A store that turns each logical commit
   into its own physical immutable unit dies of metadata. ClickHouse
   calls this the **"too many parts"** error; in Lance the exact analogue
   is **"too many versions"** (one manifest per commit, one fragment per
   tiny append, scan-time fragment fan-out). The cure is always the same:
   coalesce many logical commits into one physical unit.

3. **Deletes are logical markers, reclaimed at compaction.** No store
   physically erases a row on the delete path — that would force a
   rewrite. RocksDB writes a delete-marker key; ClickHouse sets a
   `_row_exists = 0` mask (lightweight `DELETE`); Lance carries deletion
   vectors. The dead bytes are reclaimed *later*, during compaction. —
   *In `kv-lance`:* a delete becomes a **tombstone row** (`tombstone =
   true`), filtered out at read time (`schema.rs::build_get_predicate`,
   `build_range_predicate`).

4. **A monotonic counter is the MVCC spine, decoupled from physical
   files.** Visibility ("which writes can this reader see?") is governed
   by a logical sequence number, *not* by which SST/part/fragment a row
   landed in. RocksDB's sequence number, an HLC timestamp, a Lance
   manifest version — all the same idea: a monotone integer that orders
   commits independently of physical layout.

5. **Compaction is the one unifying GC.** All the deferred work —
   merging tiny units, materialising delete masks, dropping shadowed
   versions, reshaping fragments — is collected into a single background
   process. There is exactly one GC, and it is compaction. — *In
   `kv-lance`:* `background_optimizer.rs` (`compact_files` +
   `cleanup_old_versions`).

These five are the lens for everything below. The whole point of
"gridlake" is that **Lance already gives you a versioned columnar lake;
we just need to bolt the row-sequential durable front-end (WAL+memtable)
onto it and let one compactor sweep behind.**

---

## 2. Mapping the pattern onto `kv-lance`

The hot path is already wired (the default `WritePath::LsmWithWal`). Here
is the role-for-role correspondence, with the real symbols.

| Pattern role | RocksDB term | `kv-lance` symbol (file ~line) | Notes |
| ------------ | ------------ | ------------------------------ | ----- |
| Durable sequential log | WAL | `wal.rs::Wal` — `append` (fsync, ~L131), `replay` (~L182), `truncate_to` (~L267) | length-prefixed CBOR `WalRecord`; fsync per append |
| In-memory write buffer | memtable | `memtable.rs::Memtable` — `DashMap<Key, MemtableEntry>` (~L61) | sharded; last-write-wins by `generation` |
| Flush trigger | memtable→SST flush | `flusher.rs::flusher_loop` + `do_flush` (~L131/~L205) | tick / size / shutdown triggers |
| Columnar unit build | SST / part build | `flusher.rs::single_lance_commit` → `MergeInsertBuilder::execute_reader` (~L260) | one `merge_insert` = one Lance version |
| Write-group leader | group-commit leader | `commit_gate.rs::CommitGate` (BUNDLE coalescing, ~L125) | **alternative route** — see §3.3 |
| Delete marker | delete tombstone | tombstone row (`tombstone = true`) via `build_tombstone_batch_lance` (`mod.rs` ~L1047) | folded into the same `merge_insert` |
| MVCC spine | sequence number | `version: UInt64` column (`schema.rs` ~L54) + `Memtable::generation` (`AtomicU64`, ~L67) | **flush-granular today — see §5** |
| Snapshot read | superversion | `Dataset::checkout_version` (`mod.rs::get`/`scan_impl`; `timeline.rs::view_at`) | immutable Lance snapshot |
| Compaction / merge | leveled compaction | `background_optimizer.rs` — `compact_files` (~L145) + `cleanup_old_versions` (~L196) | the single GC |
| Time-travel surface | — (RocksDB has none) | `timeline.rs::Timeline` / `TimelineView` (read-only) | **where Lance beats RocksDB** |

### 2.1 The two write paths (`config.rs::WritePath`)

`kv-lance` ships **two** commit strategies, selected at `Datastore::new`
from `LanceConfig::write_path` (`config.rs` ~L302):

- **`WritePath::LsmWithWal` (default).** The full pattern above. Commit =
  WAL fsync + memtable insert + notify flusher, then return `Ok`
  (`mod.rs::commit_lsm` ~L936). Lance is updated asynchronously by the
  flusher. **Isolation: read-committed** (reads see `memtable[now] ∪
  lance[latest]`). **Throughput: WAL-fsync-bounded per writer, scales
  with concurrency.**

- **`WritePath::LegacyCommitGate`.** Each commit submits to the
  per-Datastore `CommitGate`, which batches concurrent submitters in a
  ~500 µs window and issues ONE `merge_insert` per batch
  (`commit_gate.rs::coordinator_loop`/`execute_batch`). `commit()`
  returns only after the Lance commit lands (`mod.rs::commit_legacy_gate`
  ~L978). **Isolation: strict snapshot** (reads pin to
  `checkout_version(read_version)`). **Throughput:
  Lance-commit-latency-bounded.**

Both paths converge on the *same* `single_lance_commit` shape
(`merge_insert` keyed on `key`, writes + tombstones in one reader). The
LSM path defers it; the gate path runs it inline. The gate path is the
one a per-commit-granular timeline consumer (the "Rubicon kanban") must
use — see EPIPHANIES 2026-05-30 "timeline granularity =
write-path-dependent".

---

## 3. WAL / ACID story

This section pins **exactly where each ACID property is enforced**. The
non-obvious result: SurrealDB-on-Lance gets full ACID on the hot path
*even though Lance itself is updated asynchronously*, because durability
lives in the WAL and atomicity lives in the single `merge_insert`.

### 3.1 Atomicity — WAL group-commit + one `merge_insert` = one version

A SurrealDB transaction buffers its writes/deletes in `PendingBuffer`
(`mod.rs::Transaction::pending`). On `commit()` the buffer is partitioned
once (`pending.partition()`, ~L570) into `(writes, deletes)`. Atomicity
is then enforced at **two layers**:

1. **The WAL record is the atomic unit of durability.** `commit_lsm`
   allocates one `generation`, builds **one** `WalRecord { generation,
   ops }` carrying *all* writes and deletes of the transaction, and
   `append`s it under a single fsync (`wal.rs::append` ~L131). Crash
   recovery is all-or-nothing per record: a torn tail record (incomplete
   length prefix or body) is dropped wholesale on replay
   (`wal.rs::replay` recovery contract, ~L159–L254). So a transaction is
   either fully durable or fully absent — never half.

2. **The flush is one Lance commit = one version.** `do_flush`
   (`flusher.rs` ~L205) snapshots the memtable, partitions into writes +
   deletes, and calls `single_lance_commit` — which streams **both** a
   write batch (`build_write_batch_lance`) **and** a tombstone batch
   (`build_tombstone_batch_lance`) through a **single**
   `MergeInsertBuilder::execute_reader` keyed on `["key"]` (~L296). One
   `execute_reader` = one Lance manifest version.

> **The codex P1 fix (EPIPHANIES 2026-05-30).** Before this fix,
> `single_lance_commit` applied writes via `merge_insert` and deletes via
> a *separate* `Dataset::delete` — **two** native Lance commits. Any batch
> carrying both produced two versions: an intermediate (writes applied,
> deletes pending) and the final. The datastore write lock hid the
> intermediate from *live* readers, but `Timeline::versions()` enumerates
> raw `Dataset::versions()` and surfaced it, letting a replayer
> `view_at()` a **torn state that never atomically existed**. Folding
> deletes in as tombstone rows in the *same* `merge_insert` makes
> **1 commit = 1 version structurally, not by convention**. This is the
> single most important atomicity guarantee in the backend. (Regression:
> `test_timeline_write_delete_commit_is_single_atomic_version`; codex P1
> on PR #29, `discussion_r3328296248`.)

### 3.2 Consistency — keyed merge + tombstone read-predicate

Consistency = "a key has exactly one current value, and a deleted key
reads as absent." Two mechanisms:

- **`merge_insert` keyed on `key`** with `WhenMatched::UpdateAll` +
  `WhenNotMatched::InsertAll` (`single_lance_commit` ~L296). A memtable
  snapshot holds **exactly one op per key** (the `DashMap` overwrites by
  `generation`, `memtable.rs::insert` ~L90), so the merge source has
  **unique keys** and the upsert is well-defined — no duplicate live rows
  for a key can result from a flush.

- **Tombstone read-predicate `tombstone = false`.** Every read path
  filters tombstones out: `build_get_predicate` → `key = X'..' AND
  tombstone = false` (`schema.rs` ~L144); `build_range_predicate` →
  `... AND tombstone = false` (~L150). A deleted key therefore reads as
  absent (`get` returns `None`) even though its tombstone row physically
  persists until compaction. The in-memory layers mirror this: `get`
  treats `MemOp::Delete` as `None` (`mod.rs` ~L659); `scan_impl` overlays
  `MemOp::Delete`/`PendingEntry::Delete` as `merged.insert(k, None)`
  (~L1214–L1240).

The **read layering** that keeps consistency across the async flush is
(oldest→newest, later wins): `Lance < memtable < pending`
(`scan_impl` ~L1194). Read-your-writes is the pending buffer; committed-
but-unflushed is the memtable; durable is Lance.

### 3.3 Isolation — immutable Lance snapshot (where Lance beats RocksDB)

This is the property where the columnar/versioned substrate is *strictly
better* than a classic LSM KV store.

- **`LegacyCommitGate`** gives **strict snapshot isolation**: each
  transaction captures `read_version` at `begin` (`Datastore::transaction`
  ~L385 → `current_version`), and every read pins to
  `checkout_version(read_version)` (`get` ~L694, `scan_impl` ~L1119).
  `checkout_version` returns an **immutable, owned `Dataset`** snapshot —
  the reader cannot observe any commit that landed after `begin`.

- **Time-travel reads** (`get(key, Some(v))`) check out an *arbitrary*
  historical version on either path (`mod.rs` ~L688). The read-only
  `TimelineView` (`timeline.rs` ~L147) owns a checked-out snapshot and
  exposes `get`/`scan` only — **no** `set`/`del`/`commit`. So "SurrealDB
  is a *view* over the lake, never a store" is a **type-system
  guarantee**, not a convention (EPIPHANIES 2026-05-29).

- **`LsmWithWal`** deliberately relaxes to **read-committed**: unversioned
  reads hit Lance *@ latest* (`ds.inner.clone()`, `get` ~L692), not a
  frozen snapshot. **Why:** the flusher publishes rows into Lance
  asynchronously, so a transaction's `read_version` may be stale by the
  time the reader runs; pinning to a stale manifest would *hide* rows the
  flusher just published. Reading @ latest keeps `memtable[now] ∪
  lance[latest]` internally consistent (the long comment in `get`,
  ~L666–L699). This is the explicit throughput-for-isolation trade of the
  LSM path; the gate path is the strict-iso alternative.

> Why Lance beats RocksDB here: RocksDB snapshots are *ephemeral*
> (sequence-number pins that vanish when the snapshot handle drops, and
> cannot be re-derived after compaction GC). Lance versions are
> *first-class, durable, enumerable* (`Dataset::versions()`), so a reader
> can pin, drop, and *re-pin the same historical state hours later* — the
> basis for the `Timeline` surface RocksDB structurally cannot offer.

### 3.4 Durability — WAL fsync; manifest as the checkpoint

- **The fsync is the durability point.** `Wal::append` does
  `write_all(len) → write_all(body) → sync_all()` (`wal.rs` ~L147–L155).
  When `commit_lsm` returns, the record is on disk; a process crash before
  the flusher runs **does not lose the commit** — it replays on next open
  (`Datastore::new` replays the WAL into the memtable, `mod.rs`
  ~L301–L337).

- **The Lance manifest is the checkpoint the WAL truncates against.**
  After a flush's `merge_insert` succeeds, `do_flush` calls
  `wal.truncate_to(up_to_gen + 1)` (`flusher.rs` ~L241): every WAL record
  whose `generation ≤ up_to_gen` is now durable *in the Lance manifest*
  and can be dropped from the log. `truncate_to` rewrites a fresh sibling
  file and `rename`s it over the original — atomic on POSIX, so a crash
  mid-truncate leaves either the old or new WAL, never a torn mix
  (`wal.rs` ~L267–L344). **This is the classic WAL+checkpoint contract:
  the log is bounded by the last durable checkpoint, and the checkpoint is
  the columnar manifest.**

- **Ordering guarantee on the hot path.** `commit_lsm` appends to the WAL
  **before** inserting into the memtable (`mod.rs` ~L959–L968), so a
  reader can never observe a key whose WAL append has not yet succeeded.

**Durability test surface (as-built).** `tests.rs::lsm_recovery_*`
(~L1391, ~L1459) simulate a crash via `Box::leak` (no graceful
`shutdown`, so the in-flight flusher never drains) with
`disable_background_flusher: true` so **the WAL is the sole durability
source**. They assert that every acked write — and every delete tombstone
— survives re-open. `disable_background_flusher` is the test-only knob in
`LanceConfig` (~L358) that makes this race-free. Phase 1 (§8) extends this
into an explicit adaptive-batching durability proof.

---

## 4. ClickHouse parity

ClickHouse and `kv-lance` are the same MergeTree machine wearing different
clothes. The analogies are concrete; so are the places they break down.

### 4.1 The analogy table

| ClickHouse concept | `kv-lance` analogue | Symbol |
| ------------------ | ------------------- | ------ |
| **part** (immutable data dir) | Lance **fragment / version** | produced by `single_lance_commit` |
| `INSERT` writes a new part | a flush writes a new version | `do_flush` → `merge_insert` |
| **MergeTree background merges** | `Dataset::optimize` family | `compact_files` (`background_optimizer.rs` ~L145) |
| `async_insert` server-side buffering | the **flusher** (memtable + tick/size triggers) | `flusher_loop` + `FlusherConfig` (~L48) |
| lightweight `DELETE` → `_row_exists=0` mask | **tombstone row** + `tombstone = false` filter | `build_tombstone_batch_lance`, read predicates |
| merge materialises the mask (drops `_row_exists=0` rows) | compaction reclaims tombstone rows | §7 (ROADMAP) |
| **"too many parts"** error | **"too many Lance versions"** | invariant 2 |
| `min_insert_block_size_rows/_bytes` | flusher `max_pending_rows` (+ byte trigger, Phase 1) | `FlusherConfig` |
| sequence/mutation version | `version` column + `generation` | §5 |

### 4.2 The "≤1 insert/sec, don't out-run merges" discipline

ClickHouse's single most repeated operational rule is: **do not insert
faster than the background merges can keep up**, or parts accumulate
unboundedly and the server throws "too many parts". The standard remedy is
to batch inserts to ~1/sec (or use `async_insert` to let the server batch
for you).

The Lance-side translation is exact: **each flush is a version, each
version needs the optimizer to eventually compact it; if the flusher fires
faster than `background_optimizer` can `compact_files`, versions
accumulate** — the "too many versions" failure mode (invariant 2). The
async_insert analogue is already built (the flusher *is* server-side
buffering). What is **missing** is the *floor*: today `FlusherConfig` is
`{ tick_interval: 100ms, max_pending_rows: 1000 }` (`flusher.rs` ~L48–L66)
— there is a 100 ms latency *ceiling* and a row-count *ceiling*, but **no
minimum batch size / flush-rate floor**, so a steady trickle of one-row
commits emits a version every 100 ms regardless of how little data
accrued. Phase 1 (§8, being coded now) adds the byte-size trigger + a
flush-rate floor so the flusher coalesces aggressively under light load
and only races to flush under genuine pressure — the ClickHouse
`async_insert` *buffer* discipline, made adaptive.

### 4.3 Where the analogy breaks down (be honest)

- **`merge_insert` is an upsert, not a blind append.** ClickHouse parts
  are append-only blocks; duplicate-key resolution is deferred to
  `ReplacingMergeTree` merges or `FINAL`. `kv-lance` resolves duplicates
  **at flush time** via the keyed `merge_insert` — a memtable snapshot has
  unique keys, so there is never a "two live rows for one key, reconciled
  later" window. This is *stronger* than vanilla MergeTree (closer to
  `ReplacingMergeTree` applied eagerly), at the cost of a
  read-modify-write on the `key` column during flush.

- **One dataset, not a part-per-block free-for-all.** A ClickHouse table
  is physically many parts; a `kv-lance` Datastore is **one** Lance
  dataset whose history is a linear version chain. There is no merge-key
  fan-out across independent parts; the "merge" is fragment compaction
  within one dataset.

- **No columnar query acceleration yet.** ClickHouse's whole point is
  vectorised columnar *scans*. `kv-lance` stores **opaque binary**
  `key`/`val` (`schema.rs` ~L42) — it is a *KV store on a columnar
  substrate*, not a columnar analytics engine. Column pruning on typed
  sub-columns is a documented future extension (`schema.rs` ~L25), not a
  current capability. The parity is on the **write/merge** side, not the
  read/scan side.

- **Lance MVCC is OCC, not MVCC-on-parts.** Concurrent Lance commits use
  optimistic concurrency with retry; the `CommitGate` exists precisely to
  *collapse* concurrent commits into one so the OCC retry cascade does not
  fire (`commit_gate.rs` header, ~L26–L51). ClickHouse has no per-row OCC;
  its concurrency story is part-level and merge-level.

---

## 5. The commit-sequence (seqno) keystone

This is the deepest design point in the document. **Today the `version`
column cannot tell two coalesced transactions apart**, and the fix is a
per-row monotonic `seq`.

### 5.1 `version` is batch/flush-granular, not commit-granular

Trace where `version` is stamped:

- **Gate path:** the `CommitGate` stamps `max_version` across the whole
  batch onto *every* row in the merged `RecordBatch`
  (`execute_batch` ~L305 → `single_lance_commit(..., max_version)`), where
  each submitter's `version = read_version + 1` (`commit_legacy_gate`
  ~L990). So N transactions coalesced into one batch all receive the
  **same** stamp.

- **LSM path:** the flusher stamps the flush's `up_to_gen` onto every row
  of the flush (`do_flush` → `single_lance_commit(..., up_to_gen)`,
  `flusher.rs` ~L232). So every commit folded into one flush shares the
  **same** stamp.

In both paths, `version` is minted **per physical unit** (per batch / per
flush), which is exactly why it "mirrors physical Lance-version
granularity." That is *correct* for indexing a Lance manifest version, but
it has a hard limitation:

> **`version` cannot distinguish individual transactions that were
> coalesced into one batch/flush.** If transactions T1, T2, T3 land in one
> flush, they are indistinguishable in storage — all stamped with the same
> `version`. The `Memtable::generation` counter (`AtomicU64`, ~L67) *is*
> minted once per `commit_lsm` (~L941) and *is* per-transaction-monotonic
> — but it lives only in the WAL record and the in-memory entry; **it is
> never written to a Lance column.** It is consumed as a flush *boundary*,
> then discarded.

### 5.2 Why this costs replay/timeline fidelity

The `Timeline` surface (§3.3) walks `Dataset::versions()` and lets a
consumer `view_at(v)` each version. With flush-granular `version`, the
finest timeline resolution a replayer can reconstruct is **one entry per
flush**, not one per commit. For the Rubicon kanban — which wants *each*
commit/plan/prune as a distinct timeline entry — this forces the
`LegacyCommitGate` path (1 commit = 1 Lance version) and **forbids the
throughput win of LSM coalescing** (EPIPHANIES 2026-05-30, "timeline
granularity = write-path-dependent"). You cannot have both
high-throughput coalescing *and* per-commit replay fidelity. That coupling
is the problem.

### 5.3 The fix: a per-row monotonic `seq` (RocksDB sequence number)

Add a `seq: UInt64` column — the direct analogue of **RocksDB's per-write
sequence number** — minted **once per transaction** (the `generation`
already minted in `commit_lsm`), threaded through the WAL record into the
flush batch builders, and written to its own column. Then:

- **`version`** keeps mirroring physical Lance-version granularity (the
  manifest checkpoint index). Unchanged.
- **`seq`** carries logical commit granularity, **decoupled** from
  physical batching.

Now throughput tuning (bigger flushes, more coalescing) **never** degrades
timeline/replay fidelity: a replayer reconstructs per-commit history by
ordering on `seq`, *regardless* of how many commits shared a flush. The
`mvcc_source.rs::MvccSource` trait (`LocalGeneratedMvcc`, an `AtomicU64`
starting at 1, ~L89) is the natural home for the `seq` allocator — it
already exists as the pluggable monotonic-counter abstraction (and is
forward-designed for distributed HLC sources, ~L13–L18). This is the same
"monotonic counter is the MVCC spine, decoupled from physical files" of
invariant 4, made literal.

### 5.4 The threading challenge (honest)

This is **not** a trivial column add. The `seq` must survive **two
coalescing stages** without losing per-transaction identity:

1. **The gate's BUNDLE coalescing.** `execute_batch` (`commit_gate.rs`
   ~L291) currently merges N submissions into a `HashMap<Key, Op>` that
   **collapses by key** and keeps only `max_version`. A per-row `seq` that
   survives coalescing means: when two submissions in one batch touch the
   **same key**, the winner's `seq` must be carried (last-writer-wins,
   matching `MergeMode::Bundle`); when they touch **different keys**, each
   row keeps **its own** `seq`. The merged map's value type must grow from
   `Op` to `(Op, seq)`, and the partition step (~L318) must thread `seq`
   into the batch builder per row — not one scalar for the whole batch.

2. **The batch builders.** `build_write_batch_lance` /
   `build_tombstone_batch_lance` (`mod.rs` ~L999/~L1047) currently take a
   single scalar `version` and broadcast it across all rows
   (`UInt64Array::from(vec![version; n])`). To carry `seq`, they need a
   **per-row** `seq` array (one `seq` per `(key,val)`), i.e. the signature
   grows from `(&[(Key,Val)], version)` to a form carrying a parallel
   `&[u64]` seq slice. The flusher's `snapshot_up_to` already returns
   per-entry `generation` (`memtable.rs` ~L137) — the value is
   *available*; it just is not currently propagated into the Arrow batch.

Both stages are **additive** (new column, widened *internal* builder
signatures — the public `Transactable` trait is untouched) and stay within
the stable Lance contract (it is just another `UInt64` column in the
`merge_insert` source). The hard part is purely the internal plumbing of
per-row identity through the two coalescing points — not any Lance API
limitation. Marked **ROADMAP** (Phase 2, §8).

---

## 6. The SoA / gridlake alignment

### 6.1 The transpose tax (established)

Today the ingest side is **row-oriented** and the storage side is
**columnar**, and the seam between them pays a tax on every flush:

- **Memtable is row-oriented.** `DashMap<Key, MemtableEntry>` — one heap
  entry per key, each holding an owned `Val` (`memtable.rs` ~L61).
- **WAL is row-oriented.** `WalRecord { ops: Vec<WalOp> }` — a vector of
  per-key `Set`/`Delete` enums, CBOR-encoded (`wal.rs` ~L69–L79).
- **Storage is columnar.** Lance/Arrow `RecordBatch` — four contiguous
  typed arrays (`key`, `val`, `version`, `tombstone`).

So `do_flush` performs a **row→column transpose** on every flush: it walks
the snapshot row-by-row, pushing into `Vec<(Key,Val)>` and `Vec<Key>`
(`flusher.rs` ~L218–L227), which `build_write_batch_lance` then transposes
into columnar Arrow arrays by `.collect()`-ing iterators
(`mod.rs` ~L1017–L1022). Every byte committed is copied **twice** (into the
memtable, then into the Arrow batch) and **transposed once**.

### 6.2 The fix: make the memtable + WAL themselves SoA (CONJECTURE)

The "gridlake" alignment: make the in-memory buffer **already columnar**,
so the flush is a transcode-free concat + one upsert. The mental model:

```text
   "SoA as container"  = Arrow RecordBatch (the columnar unit)
   "stacked batches"   = the memtable (a Vec<RecordBatch>, append-only)
   "lake"              = the Lance dataset (the durable columnar store)
   "time axis"         = the version history (Timeline)
```

Concretely:

- The memtable becomes **a stack of small Arrow `RecordBatch`es** (one per
  commit, or per small group), each in the *exact* on-disk KV schema
  (`key`, `val`, `version`/`seq`, `tombstone`).
- The WAL stores those same `RecordBatch`es (Arrow IPC framing instead of
  CBOR `WalOp`), so replay rebuilds the batch stack directly.
- **Flush = `concat_batches(stack)` + one `merge_insert`** — *no
  row→column transpose*, exactly ClickHouse's "build the part in memory,
  then hand the assembled block to storage" model. The batches are already
  in storage layout; concatenation is a buffer splice, not a transpose.
- Read-your-writes keeps a small **`Key → (batch_idx, row_idx)` overlay**
  so `get`/`scan_range` still answer in O(1)/range without scanning the
  batch stack linearly. This overlay is the *only* row-oriented structure
  that survives — and it holds indices, not values.

**Benefits:** one fewer copy and zero transpose per flush; the memtable's
memory footprint is Arrow-contiguous (better cache behaviour, and a
trivially-measurable `bytes` for the Phase 1 byte trigger); the WAL record
is the *same bytes* the flush will write.

**Costs / open questions (honest):**
- Small per-commit `RecordBatch`es carry Arrow per-array overhead; very
  small commits may want a row-staging area that is *promoted* to a batch
  at a threshold (a two-tier memtable). (CONJECTURE — needs benchmarking.)
- Last-write-wins across batches moves from "DashMap overwrite" to "overlay
  points at the newest `(batch,row)`" — the overlay must be updated on
  every write and consulted on every read; correctness parity with
  `Memtable::insert`'s generation check (~L90) must be preserved.
- Range scans (`scan_range`, ~L112) over a batch stack need either the
  overlay to be ordered (BTree) or a merge across batches.

This is the largest structural change and is gated behind a new
`WritePath` variant so the row-oriented path stays the default until the
columnar path is benchmarked at parity. Marked **ROADMAP** (Phase 3, §8).

---

## 7. Compaction GC

### 7.1 The accumulation (established)

The codex P1 fix (§3.1) traded immediate space reclamation for atomicity:
the old path's separate `Dataset::delete` physically removed rows at delete
time; the tombstone-row path leaves **one dead row per created-then-deleted
key** in the dataset until something reclaims it (EPIPHANIES 2026-05-30,
"trade-off accepted"). Under a create/delete-heavy workload, tombstone rows
accumulate monotonically. They are *correct* (filtered by `tombstone =
false` on every read) but they bloat scans and storage.

### 7.2 The proposal: optimizer materialises the mask (ROADMAP)

The fix is exactly invariant 5 + invariant 3: **let the one GC (compaction)
reclaim the logical markers.** Today `background_optimizer.rs` runs
`compact_files` (fragment reshape) + `cleanup_old_versions` (version
pruning) (~L145/~L196) — it does **not** yet act on tombstone rows.

**Proposed extension:** the optimizer, on its cycle, converts tombstone
rows **older than the retention horizon** into a Lance **deletion-vector
compaction** — physically dropping both the tombstone row and the live row
it shadows, via `Dataset::delete` on a predicate like `tombstone = true AND
version < <horizon>` (then compacting the resulting deletion vectors). This
is the **direct analogue of a ClickHouse merge materialising the
`_row_exists` mask** — the mask (tombstone) is logical until a merge
(compaction) makes it physical (invariant 3 + the §4.1 row).

Why gate on a *retention horizon* (not eager): time-travel reads
(`get(key, Some(v))`, `TimelineView`) must still see the tombstone for
versions within the retention window — a delete recorded at version `v`
must read as absent when viewing `v`, which requires the tombstone row to
exist at `v`. So the GC may only reclaim tombstones older than
`LANCE_VERSION_RETENTION_SECS` (`cnf.rs` ~L40), matching the version-prune
horizon. This keeps the two GC sweeps (version prune + tombstone reclaim)
on the **same** frontier — one consistent reclamation horizon. (CONJECTURE
on the exact predicate; the *principle* — reclaim at compaction, bounded by
retention — is established by invariants 3 and 5.)

---

## 8. Phased roadmap

Each phase carries an explicit status tag. **Audited against the tree at
the time of writing** (sibling CODE agent started Phase 1/2/3 at
2026-05-30T07:33Z; see `GRIDLAKE_BUILD.md`).

### Phase 1 — ClickHouse-parity adaptive batching + WAL/ACID durability test
**Status: ROADMAP → IN PROGRESS (CODE agent, this session).**

As-built today: `FlusherConfig { tick_interval: 100ms, max_pending_rows:
1000 }` (`flusher.rs` ~L48) gives a latency *ceiling* and a row-count
*ceiling* — but **no byte-size trigger and no flush-rate floor**, so a
trickle of tiny commits emits a version every 100 ms (the "too many
versions" exposure, §4.2). The WAL/ACID side already has `lsm_recovery_*`
tests (`tests.rs` ~L1391/~L1459) proving the WAL alone recovers acked
writes + delete tombstones across a simulated crash.

Phase 1 work (in flight):
- **Adaptive batching:** add a **byte-size** trigger and a **flush-rate
  floor / minimum batch** to `FlusherConfig` so the flusher coalesces under
  light load (≈ClickHouse `async_insert` buffering /
  `min_insert_block_size_*`) and only races under genuine pressure. Add the
  byte accounting (`key.len()+val.len()`, the same metric `scan_impl`
  already uses for `ScanLimit::Bytes`, ~L1272).
- **WAL/ACID durability proof:** extend the recovery tests into an explicit
  adaptive-batching durability test (acked-commit survival under the new
  triggers).

> **Audit note (re-tag on landing).** As of writing, `FlusherConfig` still
> has only the two fields above — Phase 1 is **not yet committed**. Re-tag
> **IMPLEMENTED** only after `grep` confirms the byte/rate-floor fields
> exist in `flusher.rs`/`config.rs`, and update the `FlusherConfig`
> citation accordingly.

### Phase 2 — Per-row `seq` column
**Status: ROADMAP.**

Add `seq: UInt64`, minted per-transaction from
`mvcc_source::LocalGeneratedMvcc`, threaded through the WAL record and
**both** coalescing stages (gate BUNDLE map + batch builders) into its own
Arrow column. Decouples logical commit/replay granularity from physical
batching (§5). Additive; stays within the stable Lance contract. The
threading through `execute_batch`'s key-collapse and the
`build_*_batch_lance` per-row arrays is the real work (§5.4).

### Phase 3 — Columnar / SoA memtable behind a `WritePath` variant
**Status: ROADMAP.**

Make the memtable + WAL natively Arrow `RecordBatch`es; flush =
`concat_batches` + one `merge_insert`, transpose-free (§6). New `WritePath`
variant (e.g. `LsmColumnar`) so the row-oriented path stays default until
benchmarked at parity. Keep a `Key → (batch,row)` overlay for
read-your-writes.

### Phase 4 — Compaction GC + version backpressure
**Status: ROADMAP.**

- **Tombstone GC:** optimizer reclaims tombstone rows older than the
  retention horizon via deletion-vector compaction (§7).
- **Version backpressure:** close the ClickHouse "don't out-run merges"
  loop — if the optimizer falls behind (version count over a high-water
  mark), apply backpressure to the flusher (lengthen `tick_interval` /
  raise the min-batch floor) so ingest cannot create versions faster than
  compaction reclaims them. This is the missing *control loop* that turns
  the §4.2 discipline from advice into a guarantee.

### Phase status summary

| Phase | Title | Status |
| ----- | ----- | ------ |
| 1 | Adaptive batching + WAL/ACID durability test | ROADMAP → IN PROGRESS (CODE agent) |
| 2 | Per-row `seq` column | ROADMAP |
| 3 | Columnar/SoA memtable (`WritePath` variant) | ROADMAP |
| 4 | Compaction GC + version backpressure | ROADMAP |

> **Already shipped** (prior sessions, *not* part of this roadmap): the
> LSM+WAL+memtable+flusher hot path, the dual `WritePath`, the codex P1
> single-version atomicity fix, the read-only `Timeline`/`TimelineView`,
> the background optimizer (`compact_files` + `cleanup_old_versions`), and
> WAL replay-resilience hardening (the three corruption-mode tests in
> `wal.rs`).

---

## 9. Faithfulness constraints

Hard constraints every phase must honour — these are *policy*, not
preference.

### 9.1 Stable Rust only

Org policy is **99% stable** (nightly is used *only* for Miri in the
`ndarray` fork). The build toolchain is pinned **stable** (1.95,
`rust-toolchain.toml`). Nothing in the roadmap may require a nightly
feature. (The `mvcc_source` trait uses `impl Future` in trait position via
stable RPITIT — that is fine on 1.95.)

### 9.2 Depend ONLY on the stable Lance contract

The backend leans on a **small, stable** surface of Lance/LanceDB so a
Lance 6→7 or LanceDB 0.29→0.30 bump is a **recompile, not a redesign**. The
whole contract actually exercised in-tree:

| Stable Lance API | Used by |
| ---------------- | ------- |
| `Dataset::versions()` | `timeline.rs::versions` |
| `Dataset::checkout_version(u64)` | `get`, `scan_impl`, `timeline.rs::view_at` |
| `MergeInsertBuilder` + `WhenMatched`/`WhenNotMatched` + `execute_reader` | `single_lance_commit` (both paths) |
| deletion vectors / `Dataset::delete` | tombstone GC (Phase 4) |
| `optimize::compact_files` + `cleanup::cleanup_old_versions` | `background_optimizer.rs` |
| `Scanner` (`filter`/`project`/`order_by`/`try_into_stream`) | all read paths |

Phases 1–4 add **no new Lance API dependency** — `seq` is just another
`UInt64` column in the existing `merge_insert` source; the columnar
memtable uses the *same* `RecordBatch`/`merge_insert` already in use;
tombstone GC uses `Dataset::delete` + the existing optimizer. The schema is
deliberately minimal opaque-binary (`schema.rs` ~L42) precisely to keep
this contract narrow.

### 9.3 No new dependencies

Per `CLAUDE.md` ("Don't add dependencies without confirmation"). Everything
the roadmap needs already exists in the dep tree: `arrow_array` /
`arrow_schema` (pinned `57`, the same version Lance uses internally —
Sprint R unification, `mod.rs` ~L204), `dashmap`, `tokio`, `ciborium`,
`chrono`, `hex`, `futures`. Arrow IPC for the columnar WAL (Phase 3) is
within the existing `arrow` family — **confirm** the IPC sub-crate is
already pulled before relying on it (CONJECTURE until verified).

### 9.4 CI / rustfmt reality (EPIPHANIES 2026-05-30)

Two environmental facts constrain how work is *verified*:

- **No GitHub Actions on this fork.** PR #29 head has **zero** check runs;
  `ci.yml` triggers on every `pull_request` but Actions is not
  enabled/approved on the AdaWorldAPI fork. **The only merge gate is the
  review bots + the human owner — there is no test/clippy/fmt
  enforcement.** Consequence: agents **must** self-verify locally
  (`cargo check -p surrealdb-core --features kv-lance` + targeted
  `kvs::lance` tests) because nothing downstream will.

- **`.rustfmt.toml` is split-brain.** It enables nightly-only options
  (`wrap_comments`, `imports_granularity`, `group_imports`,
  `comment_width`) while the build is stable — a config no gate enforces,
  i.e. pure drift. Resolution (org's 99%-stable policy): make the config
  stable-honest and lean on stable tooling (`cargo-machete` et al.). A
  one-time stable `cargo fmt` normalisation is a *separate, deliberate*
  follow-up — **do not** mix mass reformat churn into feature commits.

### 9.5 Additivity (build-coordination invariant)

Per `GRIDLAKE_BUILD.md`: stable Rust, **no new deps**, **additive** (no
breaking signature changes), **never leave the tree non-compiling**, and
agents do **not** git commit/push/checkout (the orchestrator owns git).
Every roadmap phase is structured to be additive: a new column, a new
`WritePath` variant, a new optimizer step, widened *internal* builder
signatures (the public `Transactable` trait is untouched).

---

## Appendix A — Symbol index (quick citation lookup)

Line numbers are **approximate** (sibling CODE agent is editing these
files concurrently). Verify by symbol name, not line.

| Symbol | File | ~Line | Role |
| ------ | ---- | ----- | ---- |
| `Wal::append` (fsync) | `wal.rs` | ~131 | durability point |
| `Wal::replay` (recovery contract) | `wal.rs` | ~182 | crash recovery |
| `Wal::truncate_to` | `wal.rs` | ~267 | WAL↔manifest checkpoint |
| `Memtable` (`DashMap`) | `memtable.rs` | ~61 | in-mem buffer |
| `Memtable::generation` (`AtomicU64`) | `memtable.rs` | ~67 | per-commit monotonic (→ `seq`) |
| `Memtable::insert` (LWW by gen) | `memtable.rs` | ~90 | consistency (unique key) |
| `Memtable::snapshot_up_to` | `memtable.rs` | ~137 | flush snapshot (carries per-entry gen) |
| `flusher_loop` (triggers) | `flusher.rs` | ~131 | tick/size/shutdown |
| `do_flush` (`truncate_to`) | `flusher.rs` | ~205 | migrate phase |
| `single_lance_commit` (1 version) | `flusher.rs` / `commit_gate.rs` | ~260 / ~361 | atomicity |
| `FlusherConfig` | `flusher.rs` | ~48 | Phase 1 surface |
| `CommitGate` / `execute_batch` | `commit_gate.rs` | ~125 / ~291 | gate path, BUNDLE coalesce |
| `KvSchema` + predicates | `schema.rs` | ~42 | schema + `tombstone=false` filters |
| `build_write_batch_lance` / `build_tombstone_batch_lance` | `mod.rs` | ~999 / ~1047 | batch builders (Phase 2 per-row `seq`) |
| `Transaction::commit` / `commit_lsm` / `commit_legacy_gate` | `mod.rs` | ~559 / ~936 / ~978 | write-path dispatch |
| `get` / `scan_impl` (read layering) | `mod.rs` | ~629 / ~1086 | `Lance < memtable < pending` |
| `Datastore::new` (WAL replay, spawns) | `mod.rs` | ~191 | startup |
| `WritePath` / `LanceConfig` | `config.rs` | ~302 / ~334 | path selection, `disable_background_flusher` |
| `BackgroundOptimizer` (`compact_files`, `cleanup_old_versions`) | `background_optimizer.rs` | ~145 / ~196 | the one GC |
| `Timeline` / `TimelineView` | `timeline.rs` | ~75 / ~147 | read-only time axis |
| `MvccSource` / `LocalGeneratedMvcc` | `mvcc_source.rs` | ~60 / ~89 | `seq` allocator home |
| `lsm_recovery_*` tests | `tests.rs` | ~1391 / ~1459 | durability proof base |

## Appendix B — One-paragraph executive summary

`kv-lance` turns Lance — a versioned columnar lake — into an ACID KV store
by bolting a RocksDB-shaped front-end onto it: writes are made durable by a
fsynced WAL and staged in an in-memory memtable (`commit_lsm`), then a
background flusher coalesces them into Lance via a **single** keyed
`merge_insert` per flush (`single_lance_commit`), so **one flush = one
atomic Lance version** (the codex P1 fix). Reads layer `Lance < memtable <
pending`; deletes are tombstone rows filtered by `tombstone = false`;
isolation comes free from immutable `checkout_version` snapshots (strict on
the gate path, read-committed on the LSM path), and the WAL truncates
against the Lance manifest as its checkpoint. The system is the same
ingest→migrate→compact machine as ClickHouse (parts↔versions,
async_insert↔flusher, lightweight-DELETE-mask↔tombstones, MergeTree
merges↔`Dataset::optimize`), and the roadmap closes the four remaining
gaps: adaptive batching with a flush-rate floor (Phase 1, in progress), a
per-row `seq` that decouples replay granularity from physical batching
(Phase 2), a transpose-free columnar/SoA memtable (Phase 3), and
compaction-driven tombstone GC with version backpressure (Phase 4) — all
additive, all on stable Rust and the narrow stable Lance contract.
