# Lance Feature Gates — the "lite-unified" surreal build

> **Status:** PROVEN (clean compile). Reference + troubleshooting card.
> **Branch:** `claude/jirak-math-theorems-harvest-rfii13`
> **Purpose:** the exact feature-gate invocation that drops the C++/gRPC
> storage engines and keeps a *pure-Rust columnar* surreal whose backing
> store is Lance — the substrate a `ractor` actor can subscribe to.
>
> This SUPERSEDES the stale `patches/Cargo-toml.patch.txt` (which still
> describes `kv-lance` as not-yet-wired, `lance = "1.0"`, `arrow = "55"`).
> The feature is now wired in `surrealdb/core/Cargo.toml`; the pins below
> are what is actually in tree.

---

## TL;DR — the one command

```bash
# THE no-native-toolchain proof: pure-Rust columnar core, zero cc/C++.
# No RocksDB(C++), no TiKV(gRPC), no TLS crypto backend.
cargo check -p surrealdb-core --no-default-features --features kv-lance
```

`--no-default-features` is the load-bearing flag. Without it the core
default set (`kv-mem`, `graphql`) is *added to* whatever you pass — you
would still get a clean build but you would NOT have proven that
`kv-lance` stands alone.

**Observed (core-only):** clean compile, ~5m43s cold (first build of the
lance dependency closure), seconds incremental. **Zero `cc`/C++ in the
closure** — this claim holds for the core-only command above.

### ⚠ The SDK build is NOT cc-free — TLS pulls aws-lc-sys

```bash
# Lite-unified SDK (adds the client transport surface) — NOT no-cc:
cargo build -p surrealdb --no-default-features --features kv-lance,rustls
```

This still drops the C++ *storage* engines (RocksDB) and the gRPC one
(TiKV), but `rustls` is **not** a pure-Rust choice here:
`surrealdb/Cargo.toml` enables `rustls` with the **`aws_lc_rs`** crypto
backend on non-WASM targets, and `aws-lc-sys` depends on **`cc` +
`cmake`** (verified in `Cargo.lock`). So this command can require native
build tooling and does **not** satisfy "zero cc/C++".

To prove a no-native-toolchain *SDK* build you must also swap the TLS
backend off `aws_lc_rs` (e.g. the `ring` backend the WASM target uses)
or drop TLS entirely — `rustls`'s storage-engine independence does not
extend to its crypto backend. For the storage-layer proof that this card
is about, use the **core-only** command, which has no TLS surface at all.

---

## Why these gates (the lite-unified rationale)

The 16K mailbox-SoA view runs entirely on lancedb, zero-copy, accessed
from surreal through the `kv-lance` backend. So the question is never
"surreal OR lance" — it is "which of surreal's *other* KV engines do we
drag along." The answer for the lance path: none.

| Gate | deps it pulls | toolchain | keep in lite? |
|---|---|---|---|
| `kv-mem` (core default) | `surrealmx`, `tempfile`, `ext-sort`, `affinitypool` | Rust | optional (tests) |
| `kv-rocksdb` | `rocksdb` | **C++ (`cc`, libstdc++)** | ❌ drop |
| `kv-tikv` | `tikv` client | **gRPC / tonic / protoc** | ❌ drop |
| `kv-surrealkv` | `surrealkv` | Rust | ❌ drop (not lance) |
| `kv-indxdb` | `indxdb` | WASM-only | ❌ drop |
| **`kv-lance`** | `lance`, `lance-index`, `lancedb`, `arrow-array`, `arrow-schema` | **pure Rust** | ✅ **keep** |

Dropping `kv-rocksdb` is what removes the C++ build dependency; dropping
`kv-tikv` removes the gRPC/protoc requirement. `kv-lance` brings
arrow-rs + lance + object_store transitively, all Rust.

---

## The exact pins (ground truth, `surrealdb/core/Cargo.toml`)

```toml
# [features]
default  = ["kv-mem", "graphql"]
kv-lance = ["dep:lance", "dep:lance-index", "dep:lancedb",
            "dep:arrow-array", "dep:arrow-schema"]

# the optional "agnostic build first" trait surface (zero-dep):
lance-graph = ["dep:lance-graph-contract"]

# [dependencies]  — the lance family moves in lockstep (lance-graph PR #445)
lance       = { version = "=7.0.0",  optional = true }
lance-index = { version = "=7.0.0",  optional = true }
lancedb     = { version = "=0.30.0", optional = true }
arrow-array = { version = "58",      optional = true }
arrow-schema= { version = "58",      optional = true }
lance-graph-contract = { workspace = true, optional = true }
```

```toml
# surrealdb/Cargo.toml (SDK crate) — re-export gate
kv-lance = ["surrealdb-core/kv-lance", "tokio/time"]
```

**Pin discipline (lance-graph CLAUDE.md P0):** `lance`/`lance-index` are
exact-pinned `=7.0.0` because `lancedb 0.30.0` transitively requires
`lance =7.0.0`; `arrow` is `58`. These MUST match the
`AdaWorldAPI/lance-graph` workspace pin — if lance-graph bumps, this
backend bumps in lockstep, never independently. Never substitute a
crates.io lance for a fork build to make a compile pass.

---

## Troubleshooting matrix

| Symptom | Cause | Fix |
|---|---|---|
| `error: linker cc not found` / libstdc++ link errors | a C++ backend (`kv-rocksdb`) is in the feature set | confirm `--no-default-features`; do not pass `kv-rocksdb` |
| `failed to run protoc` | `kv-tikv` pulled gRPC | drop `kv-tikv`; lite-unified never needs it |
| `cc`/`cmake` invoked despite no `kv-rocksdb` | the **SDK** `rustls` feature pulls `aws-lc-sys` (cc+cmake) for the `aws_lc_rs` TLS backend — unrelated to storage | use the core-only command for the no-cc proof; for a no-cc SDK, switch rustls to the `ring` backend or drop TLS |
| `Patch lance ... was not used in the crate graph` | fork wiring / transitive semver mismatch | check direct `[patch]` + `Cargo.lock`; resolve the transitive blocker, do NOT fall back to crates.io (P0) |
| `lancedb 0.30 requires lance =7.0.0 but =X found` | lance family drifted out of lockstep | re-pin all three (`lance`, `lance-index`, `lancedb`) to the lance-graph workspace versions together |
| build "works" but kv-lance never exercised | default features silently re-added `kv-mem` | the proving build MUST use `--no-default-features` |
| huge first-build time, then fast | cold lance/arrow closure | expected (~5–6 min cold); incremental is seconds |

---

## The ractor connection (why this card lives next to the ractor fix)

The lite-unified surreal is the *storage half*; `ractor` is the *actor
half* of the self-updating substrate. The breakthrough loop:

```
Lance Dataset::versions()                     (kv-lance backend, this card)
      │  new version committed
      ▼
LanceVersionScheduler  (ractor actor, bounded mailbox)
      │  KanbanMove { target: ExecTarget::Jit }
      ▼
jitson / Cranelift formula                    (compute, NOT a query → no DataFusion)
      │  produces a SoA tenant delta
      ▼
MailboxSoaView tenant write                   (zero-copy into the Lance-backed column)
      │  commits a new Lance version  ──────► loop closes
```

- **DataFusion is NOT on this path.** The loop is *compute* (formula
  evaluation), not *query* (SQL planning). `lance`/`lancedb`/`arrow` ARE
  on the path — they are the zero-copy columnar store, not the planner.
- **Why the ractor messaging fix matters here.** A
  `LanceVersionScheduler` actor that fans `KanbanMove`s to a bounded
  worker mailbox will surface `MessagingErr::Saturated(T)` as graceful
  backpressure (from `try_send` on a full bounded mailbox — distinct
  from `SendErr`, which means the actor is dead). Before the fix, three
  `match` sites in ractor were non-exhaustive on `Saturated` and the
  crate did not compile on default features. Fixed on the same branch
  (`claude/jirak-math-theorems-harvest-rfii13`):
  - receive-side loops (`actor.rs`, `thread_local/inner.rs`): Saturated
    cannot occur receive-side → treat like closed channel (`Signal::Kill`).
  - `derived_actor.rs::get_derived`: deconvert + re-wrap as `Saturated`,
    mirroring the `SendErr` arm — so a `DerivedActorRef` carrying the
    subset `KanbanMove` type propagates backpressure with the typed
    message intact (the scheduler can retry-with-backoff or escalate).

So: **bounded mailbox + `Saturated` = the backpressure valve** between a
fast Lance commit stream and a slower jitson compute worker. The lite
surreal supplies the version stream; ractor supplies the
ownership-safe, backpressure-aware dispatch.

---

## What this card does NOT claim

- Not a benchmark. The 5m43s is a cold-compile wall-clock, not a
  throughput number. Any "faster" claim needs a measured reproducer
  (truth-architect gate).
- Not a green-light to wire the loop into a real Cargo.toml. The
  `surreal_container` consumer is still `BLOCKED(C)` — the
  `AdaWorldAPI/surrealdb` fork dep (this `kv-lance` feature) is not yet
  added to `surreal_container/Cargo.toml`. That single wiring step +
  the loop above is the remaining work, not a 12-day lift.
