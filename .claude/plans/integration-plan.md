# Integration Plan: surrealdb's role in the four-repo convergence

**This repo**: `AdaWorldAPI/surrealdb` — awareness layer + multi-model storage + KV abstraction.

**Status**: planning document. Companion plans at the same path in the other repos:
- `AdaWorldAPI/lance-graph:.claude/plans/integration-plan.md`
- `AdaWorldAPI/sea-orm:.claude/plans/integration-plan.md`
- `AdaWorldAPI/ndarray:.claude/plans/integration-plan.md`

---

## 1. The convergence target

The goal across all four repos:

> *Foundry-style ontology + BEAM-style supervision + ClickHouse-style analytic + Postgres-style ACID + cognitive primitives — all on one Arrow substrate, surfaced to consumers as a typed sea-orm API.*

Four glue crates close the gap:

| # | Glue crate | Owner repo | Bridges |
|---|---|---|---|
| 1 | `surrealdb-ractor` | **this repo** | `cf` / live queries → ractor mailboxes |
| 2 | `lance-graph-tikv-provider` | lance-graph | TiKV ranges → Arrow `TableProvider` |
| 3 | `sea-orm-ractor` | sea-orm | `Entity::PK` → ractor process registry |
| 4 | `cognitive-shader-actor` | lance-graph | cognitive shaders → `ractor::Actor` adapter |

This repo owns **#1** plus two new feature-gated additive paths: `kv-tikv-native-mvcc` (Percolator HLC as the `version` source) and `lance-projection` (CDC-fed columnar projection that runs **alongside**, not in place of, `kv-lance`).

### Integration principle: additive contract shape

**All work in this plan is additive.** No existing trait signature changes. No existing module moves. No existing file deletes — in particular, `core/src/kvs/lance/` and its handrolled WAL / memtable / flusher / commit_gate stay exactly as-is. New capabilities ship as **new feature flags + new modules** consumers opt into. Old surfaces stay supported. Any deprecation runway is signposted but out of scope here — five+ versions before any old surface is touched.

**Contract crates are the integration surface.** Cross-engine vocabulary lives in zero-dep trait crates any consumer can pin without bringing heavy implementations. New capability = new trait. New trait = optional dep.

### Contracts (existing + new)

| Contract | Owner repo | Status today | This plan adds |
|---|---|---|---|
| `lance-graph-contract` | lance-graph | 0.1.x, wired via `lance-graph` feature in `core/Cargo.toml:69` | pin 0.2.0 once its additive submodules land |
| `KVKey` / `KVValue` / `Datastore` / `Transaction` | **this repo** | stable (`core/src/kvs/key.rs`, `kvs/api.rs`) | **unchanged** — new traits `CfStream` and `MvccSource` added alongside |
| `EntityTrait` / `Select<E>` | sea-orm | 2.0 | unchanged — sea-orm-ractor adds `EntityActor` |
| `ndarray::hpc::*` | ndarray | 0.17 fork | unchanged |

**New traits this repo adds** (all in new modules, all additive):

```rust
// core/src/cf/stream.rs              — NEW (consumed by surrealdb-ractor)
/// Strongly-typed cf delta stream. Existing cf cursor APIs unchanged;
/// this is a thin Arrow-shaped wrapper for actor consumption.
pub trait CfStream {
    fn next_delta(&mut self) -> impl Future<Output = Option<anyhow::Result<LiveDelta>>>;
}

// core/src/kvs/mvcc_source.rs        — NEW (impl'd by kv-tikv-native-mvcc)
/// Where does the u64 version come from? Existing backends use a locally
/// generated counter (default impl); kv-tikv-native-mvcc impls this to
/// use PD HLC. Other backends keep working without changes.
pub trait MvccSource {
    fn next_version(&self) -> impl Future<Output = anyhow::Result<u64>>;
}
```

Neither trait modifies an existing surface. Existing `Datastore` / `Transaction` impls do not need to implement them — they're opt-in by feature flag.

**Per-repo enforcement**: every Sprint item below is read as "add this; don't change what's there." Where an earlier framing said "demote kv-lance," it has been rewritten as "add lance-projection alongside" (§6).

---

## 2. Architecture diagram

```
                ┌──────────────────────────────────────────┐
                │              consumer crate              │
                └──────────────────┬───────────────────────┘
                                   │ typed entities
                                   ▼
                ┌──────────────────────────────────────────┐
                │            sea-orm-arrow 2.0             │
                └────┬─────────────────┬───────────────┬───┘
                     │                 │               │
                     ▼                 ▼               ▼
              ┌───────────┐     ┌───────────┐    ┌───────────┐
              │  ractor   │◄────│ THIS REPO │    │lance-graph│
              │ (actors,  │ #1  │  (cf +    │    │ (Cypher,  │
              │ mailboxes,│     │   live    │    │ ontology, │
              │ supervis.)│     │  queries) │    │cognitive) │
              └─────┬─────┘     └─────┬─────┘    └─────┬─────┘
                    │ #3              │                │ #2,#4
                    ▼                 ▼                ▼
              ┌─────────────────────────────────────────────┐
              │       TiKV substrate (Raft + Percolator)    │
              └─────────────────────────────────────────────┘
                                  │
                                  ▼
                    ┌────────────────────────────┐
                    │  ndarray fork (SIMD HPC)   │
                    └────────────────────────────┘
```

---

## 3. Role of surrealdb in the integration

SurrealDB is **the awareness layer**:

- `core/src/cf/` change feeds emit ordered deltas across all models (doc, graph, KV, TS)
- `LIVE SELECT` lets consumers subscribe to ongoing changes
- Multi-model means one subscription covers heterogeneous events

Combined with ractor mailboxes (glue #1), this is the substrate equivalent of **OTP message-passing with a transactional analytic backbone**. The Hiro-ticket-system pattern in `almato/bardioc` is realisable as: surrealdb cf → surrealdb-ractor → sea-orm-ractor dispatch → entity-scoped actor.

---

## 4. Current state — file-by-file

### `core/src/cf/`
Change-feed module. **Bedrock of the awareness layer.** Glue #1 hooks into this via the new `CfStream` trait (added in a new sibling module). The existing cf API is unchanged.

### `core/src/kvs/lance/`
Handrolled LSM-on-Lance — `wal.rs`, `memtable.rs`, `flusher.rs`, `commit_gate.rs`, `tx_buffer.rs`, `schema.rs`, `background_optimizer.rs`. The 4-column opaque-binary schema (`schema.rs:1-30`):

```
key:       Binary       — opaque SurrealDB binary key
val:       Binary       — opaque SurrealDB binary value
version:   UInt64       — MVCC point-lookup convenience
tombstone: Boolean      — explicit deletion marker
```

**This module stays exactly as-is.** It works, it ships, it has tests. The architectural critique stands (Lance is the wrong shape for transactional row-MVCC), but the additive answer is to add a **sibling** path (`lance-projection`, §6) and let consumers choose, not to delete `kvs/lance/`.

The `schema.rs:14-22` TODO ("parse `key` into typed sub-columns") remains a future optimisation; not in this plan's scope.

### `core/src/kvs/tikv/`
TiKV adapter. Today generates its own `u64` generation. **Stays unchanged**. A new feature `kv-tikv-native-mvcc` (§5b) adds a sibling transaction implementation that uses PD HLC timestamps; the default path remains the local generator so existing deployments are unaffected.

### `core/src/kvs/key.rs`
`KVKey` / `KVValue` traits, `storekey`-encoded for prefix-scan ordering. **Stable** — no change.

### `core/Cargo.toml`
Pulls `lance-graph-contract` via the `lance-graph` feature (line 69). **Plan**: pin `lance-graph-contract = "0.2"` once that ships with the additive IR submodule (lance-graph plan §7 Sprint 0). Existing `kv-lance` feature stays exactly as-is.

### `core/src/idx/trees/vector.rs`
Uses ndarray fork's SIMD distance kernels via `vector-hpc` feature. **Stable.**

---

## 5. Glue #1 — `surrealdb-ractor`

**Goal**: surrealdb's cf events / live query results route into ractor mailboxes automatically.

**Why**: without this, every actor would have to poll surrealdb for changes. The "awareness layer" is only awareness if consumers don't have to ask.

**Additive shape**: NEW top-level crate `surrealdb-ractor/`. Implements consumption of the new `CfStream` trait. Existing cf cursor APIs unchanged. surrealdb-core does not depend on it; consumers depend on both.

### API sketch

```rust
// surrealdb-ractor/src/lib.rs
use ractor::ActorRef;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use arrow_array::RecordBatch;
use futures::Stream;

#[derive(Debug, Clone)]
pub enum LiveDelta {
    Create(RecordBatch),
    Update { before: RecordBatch, after: RecordBatch },
    Delete(RecordBatch),
}

impl LiveDelta {
    pub fn primary_key<T: TryFrom<arrow_array::ArrayRef>>(&self, name: &str)
        -> anyhow::Result<T>
    { /* ... */ unimplemented!() }

    pub fn into_record_batch(self) -> RecordBatch {
        match self {
            LiveDelta::Create(b) | LiveDelta::Delete(b) => b,
            LiveDelta::Update { after, .. } => after,
        }
    }
}

pub struct LiveQueryRouter<M: ractor::Message + From<LiveDelta>> {
    pub db: Surreal<Any>,
    pub query: String,
    pub actor: ActorRef<M>,
}

impl<M: ractor::Message + From<LiveDelta>> LiveQueryRouter<M> {
    pub async fn run(self) -> anyhow::Result<()> {
        let mut stream = surreal_live_stream(&self.db, &self.query).await?;
        while let Some(delta) = stream.next().await {
            self.actor.send_message(M::from(delta?))?;
        }
        Ok(())
    }
}

pub async fn live_stream(query: &str)
    -> anyhow::Result<impl Stream<Item = anyhow::Result<LiveDelta>>>
{ /* ... */ unimplemented!() }
```

### Wiring inside surrealdb-core

The `cf` module already produces typed change events. The bridge (in a NEW sibling module `core/src/cf/stream.rs`):
1. Implements `CfStream` over the existing cf cursor
2. Converts events into Arrow `RecordBatch` using the catalog-derived schema
3. Emits as `LiveDelta`

The existing cf cursor API stays untouched; the new module is an adapter.

### Backpressure

ractor mailboxes are bounded. The router awaits `send_message`; if the mailbox is full, the cf consumer yields. Correct semantics for slow consumers.

### Rubicon transaction model

- **Pre-decisional state** lives in the actor's mailbox / state (retractable, supervisable)
- **Crossing the rubicon** = surrealdb transaction commit
- The cf event fans out via the router to all interested actors — peer awareness is automatic

See Example 2 in §10.

---

## 5b. Glue #2 (surrealdb side) — `kv-tikv-native-mvcc`

**Goal**: surrealdb's `version: u64` uses TiKV's HLC timestamps directly when running on TiKV — **as an opt-in feature**.

**Why**: today surrealdb generates its own u64 generation. On TiKV that's a redundant clock — PD already has Percolator timestamps. Using PD's HLC means MVCC reads compose naturally across TiKV-backed and Lance-backed datasets (`snapshot_ts` in `lance-graph-tikv-provider` is the same number).

**Additive shape**: NEW feature flag `kv-tikv-native-mvcc`. Existing `kv-tikv` feature unchanged. When `kv-tikv-native-mvcc` is on, the adapter implements the new `MvccSource` trait to provide PD HLC; when off, current behaviour is unchanged. The default is current behaviour.

### Code touch

```rust
// core/src/kvs/tikv/mod.rs — NEW feature-gated branch, existing path untouched

impl Datastore for TikvDatastore {
    // Existing methods unchanged.

    /// NEW additive helper: only callable when kv-tikv-native-mvcc is on.
    #[cfg(feature = "kv-tikv-native-mvcc")]
    async fn begin_with_native_mvcc(
        &self,
        read_version: Option<u64>,
    ) -> Result<Box<dyn Transaction>> {
        let snapshot_ts = match read_version {
            Some(v) => v.into(),
            None => self.client.current_timestamp().await?,
        };
        let txn = self.client.snapshot(snapshot_ts, Default::default());
        Ok(Box::new(TikvNativeMvccTxn::new(txn)))
    }
}
```

The existing `Datastore` trait is unchanged — `begin_with_native_mvcc` is a new inherent method, not a trait method. Consumers opt in by feature + by calling it explicitly.

---

## 6. Adding `lance-projection` alongside `kv-lance` (additive)

**Goal**: add a CDC-fed columnar projection path **as a sibling** of `kv-lance`, not as a replacement.

**Why**: the design discussion concluded Lance is the wrong shape for transactional row-MVCC KV but the right shape for *columnar projection of state owned by another backend*. The additive answer is to provide both:

- `kv-lance` (existing) — stays as-is. Same 4-column schema, same WAL + memtable + flusher. Consumers that already depend on it keep working.
- `lance-projection` (new) — a CDC consumer that subscribes to cf and writes a typed-column Lance dataset alongside whatever the source backend is.

Both can coexist in one deployment. `kv-lance` remains a primary backend choice; `lance-projection` is an analytical view fed by any other backend (RocksDB / SurrealKV / TiKV).

### What's new (nothing existing changes)

- **New feature flag** `lance-projection` in `core/Cargo.toml` — sibling to `kv-lance`
- **New module** `core/src/projection/lance/` — sibling to `core/src/kvs/lance/`
- **New `LanceProjectionRefresher`** — consumes the new `CfStream` trait, writes typed Arrow batches to Lance via `Dataset::append`, uses Lance's native versioning at refresh boundaries
- **Typed columns by default** — schema read from `lance-graph-catalog` (lance-graph plan §7 Sprint 3)

### What stays exactly as-is

- `core/src/kvs/lance/wal.rs` — untouched
- `core/src/kvs/lance/memtable.rs` — untouched
- `core/src/kvs/lance/flusher.rs` — untouched
- `core/src/kvs/lance/commit_gate.rs` — untouched
- `core/src/kvs/lance/tx_buffer.rs` — untouched
- `core/src/kvs/lance/schema.rs` — untouched (still 4-column opaque-binary for kv-lance role)
- `core/src/kvs/lance/background_optimizer.rs` — untouched
- The `kv-lance` feature flag — untouched, still ships

### CDC consumer sketch (`lance-projection`)

```rust
// core/src/projection/lance/refresher.rs — NEW module
use arrow_array::RecordBatch;
use lance::Dataset;
use lance_graph_catalog::NodeShape;
use crate::cf::stream::CfStream;

pub struct LanceProjectionRefresher<S: CfStream> {
    source: S,                          // any CfStream impl: RocksDB/SurrealKV/TiKV
    target: Dataset,                    // Lance dataset for this projection
    shape: NodeShape,                   // typed schema from catalog
    refresh_interval: std::time::Duration,
    last_version: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<S: CfStream> LanceProjectionRefresher<S> {
    /// Long-running task: tail cf cursor via CfStream and append to Lance.
    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            let mut batch_events = Vec::new();
            while let Some(event) = self.source.next_delta().await {
                batch_events.push(event?);
                if batch_events.len() >= 1000 { break; }
            }
            if batch_events.is_empty() { continue; }
            let arrow_batch = self.shape.encode_deltas(&batch_events)?;
            self.target.append(arrow_batch, Default::default()).await?;
            if let Some(last) = batch_events.last() {
                self.last_version.store(
                    last.version(),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
    }
}
```

### Consumer perspective

```toml
# A deployment that uses RocksDB as primary KV AND maintains a typed Lance projection:
[dependencies]
surrealdb-core = { version = "...", features = ["kv-rocksdb", "lance-projection"] }

# A deployment that still uses kv-lance (legacy POC) — unchanged:
surrealdb-core = { version = "...", features = ["kv-lance"] }

# A deployment that uses both (kv-lance for legacy data + lance-projection for new):
surrealdb-core = { version = "...", features = ["kv-lance", "lance-projection"] }
```

Features compose. No consumer is forced off `kv-lance`. The architectural fix is delivered via choice, not replacement.

### What's gained without breaking anything

- New consumers get the typed-column path with Lance's native versioning
- lance-graph reads the typed projection via its TableProvider (lance-graph plan §5)
- Existing kv-lance deployments are not migrated
- A future deprecation cycle (out of this plan's scope) can sunset `kv-lance` after the projection path has matured — but only with a five+-version runway, and only if there are no active consumers

---

## 7. Catalog-driven `DEFINE` codegen

`lance-graph-catalog` becomes the source of truth via **new methods** added to `Catalog` (lance-graph plan §7 Sprint 3). This repo consumes it by adding `surrealdb-cli generate --from-ontology schema.yml` that emits `DEFINE` statements.

Additive: new CLI subcommand, existing CLI surface unchanged.

Given the shared single source:

```yaml
nodes:
  Person:
    pk: id
    columns: { id: UInt64, name: String, age: UInt32, email: { type: String, unique: true } }
edges:
  KNOWS: { src: Person, dst: Person, properties: { since: Date } }
```

Emits:

```sql
DEFINE TABLE person SCHEMAFULL;
DEFINE FIELD id    ON person TYPE int;
DEFINE FIELD name  ON person TYPE string;
DEFINE FIELD age   ON person TYPE int ASSERT $value >= 0;
DEFINE FIELD email ON person TYPE string;
DEFINE INDEX person_email ON person FIELDS email UNIQUE;
DEFINE TABLE knows SCHEMAFULL TYPE RELATION FROM person TO person;
DEFINE FIELD since ON knows TYPE datetime;
```

Round-trip tested with the sea-orm + lance-graph sides.

---

## 8. Sprint sequence (this repo)

All sprints are **additive** — nothing existing changes signature, nothing existing moves, nothing existing is deleted.

### Sprint 0 — `lance-graph-contract` 0.2.0 consumer (3 days)
- Pin `lance-graph-contract = "0.2"` once it ships (lance-graph plan §7 Sprint 0)
- Wire the new IR submodule into surrealdb's planner stub
- Existing `lance-graph` feature consumers compile unchanged
- E2E: a query the planner can route through `lance-graph-contract::ir` operators

### Sprint 1 — `surrealdb-ractor` MVP (2 weeks)
- New crate at `surrealdb-ractor/`
- New module `core/src/cf/stream.rs` implementing `CfStream` over the existing cursor
- Live query → ractor mailbox routing for one `(query, actor)` pair
- Integration test: counter actor counts deltas from `LIVE SELECT count(*) FROM events`
- Bench mailbox throughput vs raw cf cursor
- Existing cf cursor API unchanged

### Sprint 2 — `kv-tikv-native-mvcc` (1 week)
- Add NEW feature flag `kv-tikv-native-mvcc`
- Add NEW `MvccSource` trait in new module `core/src/kvs/mvcc_source.rs`
- Add NEW `TikvNativeMvccTxn` + inherent `begin_with_native_mvcc()` method (not a trait method change)
- Default off; existing `kv-tikv` consumers unaffected
- E2E: a transaction at snapshot `T` reads consistently against a parallel lance-graph-tikv-provider read at same `T`

### Sprint 3 — `lance-projection` sibling (3 weeks)
- Add NEW feature flag `lance-projection`
- Add NEW module `core/src/projection/lance/`
- Implement `LanceProjectionRefresher` consuming `CfStream`
- Typed columns via catalog (lance-graph plan §7 Sprint 3 outputs)
- **`core/src/kvs/lance/` is not touched** — both modules coexist
- E2E: a write to kv-rocksdb is visible in the lance-projection within `refresh_interval`

### Sprint 4 — federation E2E (2 weeks)
- Together with sea-orm Sprint 4 and lance-graph Sprint 4: consumer using sea-orm-arrow against a federated planner across all three engines

---

## 9. Examples

### Example 1 — Live query → ractor mailbox (event counter)

```rust
use ractor::{Actor, ActorRef, ActorProcessingErr};
use surrealdb_ractor::{LiveQueryRouter, LiveDelta};
use surrealdb::Surreal;

struct EventCounter;

#[derive(Debug)]
enum CounterMsg { Delta(LiveDelta) }

impl From<LiveDelta> for CounterMsg {
    fn from(d: LiveDelta) -> Self { CounterMsg::Delta(d) }
}

impl Actor for EventCounter {
    type Msg = CounterMsg;
    type State = u64;
    type Arguments = ();

    async fn pre_start(
        &self, _: ActorRef<Self::Msg>, _: (),
    ) -> Result<Self::State, ActorProcessingErr> { Ok(0) }

    async fn handle(
        &self, _: ActorRef<Self::Msg>, msg: Self::Msg, state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            CounterMsg::Delta(LiveDelta::Create(_)) => *state += 1,
            CounterMsg::Delta(LiveDelta::Delete(_)) => *state = state.saturating_sub(1),
            _ => {}
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db: Surreal<_> = Surreal::new("mem://").await?;
    let (actor, _h) = Actor::spawn(None, EventCounter, ()).await?;
    LiveQueryRouter {
        db,
        query: "SELECT * FROM events WHERE category = 'sale'".into(),
        actor,
    }.run().await
}
```

### Example 2 — Rubicon goal-state model

A goal transitions from deliberation (mailbox-local) to implementation (commit fan-out via cf).

```rust
use ractor::{Actor, ActorRef, ActorProcessingErr};
use surrealdb_ractor::LiveDelta;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

#[derive(Debug)]
enum GoalMsg {
    Deliberate { proposal: serde_json::Value },   // pre-rubicon — mailbox-local
    Commit,                                        // cross the rubicon
    PeerCommitted(LiveDelta),                      // peer awareness via cf
}

impl From<LiveDelta> for GoalMsg {
    fn from(d: LiveDelta) -> Self { GoalMsg::PeerCommitted(d) }
}

struct GoalStateActor;
struct GoalState {
    db: Surreal<Any>,
    current_proposal: Option<serde_json::Value>,
}

impl Actor for GoalStateActor {
    type Msg = GoalMsg;
    type State = GoalState;
    type Arguments = Surreal<Any>;

    async fn pre_start(
        &self, _: ActorRef<Self::Msg>, db: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(GoalState { db, current_proposal: None })
    }

    async fn handle(
        &self, _: ActorRef<Self::Msg>, msg: Self::Msg, state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            GoalMsg::Deliberate { proposal } => {
                state.current_proposal = Some(proposal);  // mailbox-local
            }
            GoalMsg::Commit => {
                if let Some(p) = state.current_proposal.take() {
                    let _: Vec<serde_json::Value> = state.db
                        .create("goals").content(p).await
                        .map_err(|e| ActorProcessingErr::from(anyhow::Error::from(e)))?;
                }
            }
            GoalMsg::PeerCommitted(_delta) => {
                // React to others crossing the rubicon.
            }
        }
        Ok(())
    }
}
```

### Example 3 — Coexistence: kv-rocksdb + lance-projection

```rust
// A deployment uses kv-rocksdb as primary (writes go here) AND maintains a
// typed columnar projection in Lance for analytic / graph queries.
//
// Both `kv-lance` and `lance-projection` features can coexist if desired.
//
// Cargo.toml:
//   surrealdb-core = { features = ["kv-rocksdb", "lance-projection"] }

let ds = Datastore::new("rocksdb:///var/lib/surreal/db").await?;

// In a background task, refresh the projection from cf.
tokio::spawn(async move {
    let cf_stream = ds.cf_stream("my_namespace.my_database").await?;
    let lance_target = lance::Dataset::open("/var/lib/surreal/projection.lance").await?;
    let shape = catalog.node_shape("Person")?;
    LanceProjectionRefresher {
        source: cf_stream,
        target: lance_target,
        shape,
        refresh_interval: Duration::from_secs(5),
        last_version: Default::default(),
    }.run().await
});

// Analytic / graph queries hit the projection (lance-graph TableProvider).
// OLTP writes go through ds (kv-rocksdb).
```

---

## 10. Open questions

1. **`surrealdb-ractor` crate placement** — top-level here, or under `AdaWorldAPI/ractor-*`? Currently here because cf is internal.
2. **`kv-lance` long-term** — stays in this plan's scope. Future deprecation only after `lance-projection` has matured, with five+-version runway.
3. **Native MVCC fallback** — non-TiKV backends keep local u64 generation. Documented in the new feature flag's doc comment.
4. **cf cursor stability** — the new `CfStream` trait wraps the existing cursor; the wrapper insulates surrealdb-ractor from cursor-internal changes.
5. **Coexistence cost** — running `kv-lance` + `lance-projection` in the same binary roughly doubles the Lance-side dependency surface. Document in the feature flag.

---

## 11. Cross-references

- **Glue #2** (TiKV-side TableProvider): `AdaWorldAPI/lance-graph:.claude/plans/integration-plan.md` §5
- **Glue #3** (sea-orm-ractor): `AdaWorldAPI/sea-orm:.claude/plans/integration-plan.md` §5
- **Glue #4** (cognitive-shader-actor): `AdaWorldAPI/lance-graph:.claude/plans/integration-plan.md` §6
- **SIMD kernels**: `AdaWorldAPI/ndarray:.claude/plans/integration-plan.md`
- **Catalog `to_*` methods** (the additive bridge between repos): `AdaWorldAPI/lance-graph:.claude/plans/integration-plan.md` §7 Sprint 3
