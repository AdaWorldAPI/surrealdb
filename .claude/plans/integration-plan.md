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

This repo owns **#1** plus two structural changes: **kv-tikv native MVCC** (Percolator HLC as the `version` source) and **demoting kv-lance to lance-projection** (CDC-fed, not a peer backend).

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

Combined with ractor mailboxes (glue #1), this is the substrate equivalent of **OTP message-passing with a transactional analytic backbone**. Neither ClickHouse nor BEAM has this natively. The Hiro-ticket-system pattern in `almato/bardioc` is realisable as: surrealdb cf → surrealdb-ractor → sea-orm-ractor dispatch → entity-scoped actor.

---

## 4. Current state — file-by-file

### `core/src/cf/`
Change-feed module. **Bedrock of the awareness layer.** Glue #1 hooks into this. No structural change planned; only the bridge crate (§5) consumes it.

### `core/src/kvs/lance/`
Handrolled LSM-on-Lance — `wal.rs`, `memtable.rs`, `flusher.rs`, `commit_gate.rs`, `tx_buffer.rs`, `schema.rs`, `background_optimizer.rs`. The 4-column opaque-binary schema (`schema.rs:1-30`):

```
key:       Binary       — opaque SurrealDB binary key
val:       Binary       — opaque SurrealDB binary value
version:   UInt64       — MVCC point-lookup convenience
tombstone: Boolean      — explicit deletion marker
```

**Architectural error today**: this treats Lance as a peer KV backend when its physical shape (immutable columnar files) is the wrong fit for transactional row-MVCC. Values are `Binary` so the columnar benefits don't materialise. The `schema.rs:14-22` TODO ("parse key into typed sub-columns") is the schema-side acknowledgement.

**Plan** (see §6): demote to `lance-projection`. Stop using as primary backend. CDC-feed from the real KV instead.

### `core/src/kvs/tikv/`
TiKV adapter. Today generates its own `u64` generation, ignoring TiKV's HLC timestamps from PD. **Plan** (see §5b): add `kv-tikv-native-mvcc` mode that uses PD timestamps as the `version` column. This is glue #2 from the surrealdb side — same `u64` flows to lance-graph's TableProvider snapshot.

### `core/src/kvs/key.rs`
`KVKey` / `KVValue` traits, `storekey`-encoded for prefix-scan ordering. **Stable** — no change.

### `core/Cargo.toml`
Pulls `lance-graph-contract` via the `lance-graph` feature (line 69). **Plan**: pin `lance-graph-contract = "0.2"` once that ships with the federated planner IR (lance-graph plan §7 Sprint 0).

### `core/src/idx/trees/vector.rs`
Uses ndarray fork's SIMD distance kernels via `vector-hpc` feature (Cargo.toml:71-77). **Stable** — depends on ndarray plan's API stability commitment.

---

## 5. Glue #1 — `surrealdb-ractor`

**Goal**: surrealdb's cf events / live query results route into ractor mailboxes automatically.

**Why**: without this, every actor would have to poll surrealdb for changes. The "awareness layer" is only awareness if consumers don't have to ask.

**Crate location**: new top-level crate `surrealdb-ractor/`

### API sketch

```rust
// surrealdb-ractor/src/lib.rs
use ractor::ActorRef;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use arrow_array::RecordBatch;
use futures::Stream;

/// A single delta emitted by a LIVE SELECT, in Arrow form.
#[derive(Debug, Clone)]
pub enum LiveDelta {
    Create(RecordBatch),
    Update { before: RecordBatch, after: RecordBatch },
    Delete(RecordBatch),
}

impl LiveDelta {
    /// Extract a primary key column from the delta.
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

/// Subscribes to a live query and routes deltas as messages to one actor.
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

/// Convenience: open a live query and return a stream of Arrow deltas.
pub async fn live_stream(query: &str)
    -> anyhow::Result<impl Stream<Item = anyhow::Result<LiveDelta>>>
{ /* ... */ unimplemented!() }
```

### Wiring inside surrealdb-core

The `cf` module already produces typed change events. The bridge:
1. Subscribes to the cf cursor for the query's tables
2. Filters per the `WHERE` clause
3. Converts each event into an Arrow `RecordBatch` using the catalog-derived schema (so the actor receives the same Arrow shape lance-graph would)
4. Sends as a `LiveDelta`

### Backpressure

ractor mailboxes are bounded. The router awaits `send_message`; if the mailbox is full, the cf consumer yields. **This is correct** — slow consumers slow producers, no unbounded queueing.

### Per-actor filtering

A live query expression *is* the filter. One router per `(query, actor)` pair. Many routers share one surrealdb connection.

### Rubicon transaction model

The live-query → mailbox path enables the Heckhausen Rubicon model directly:

- **Pre-decisional state** lives in the actor's mailbox / state (retractable, supervisable)
- **Crossing the rubicon** = surrealdb transaction commit
- The cf event fans out via the router to all interested actors — *peer awareness of decisions is automatic*

See Example 2 in §9.

---

## 5b. Glue #2 (surrealdb side) — `kv-tikv-native-mvcc`

**Goal**: surrealdb's `version: u64` uses TiKV's HLC timestamps directly when running on TiKV.

**Why**: today surrealdb generates its own u64 generation in `kvs/lance/wal.rs` and equivalents. On TiKV that's a redundant clock — PD already has Percolator timestamps. Using PD's HLC means MVCC reads compose naturally across TiKV-backed and Lance-backed datasets (the `snapshot_ts` in `lance-graph-tikv-provider` is the same number).

### Code touch

```rust
// surrealdb/core/src/kvs/tikv/mod.rs — new feature-gated mode

impl Datastore for TikvDatastore {
    async fn begin_with_version(
        &self,
        read_version: Option<u64>,
    ) -> Result<Box<dyn Transaction>> {
        #[cfg(feature = "kv-tikv-native-mvcc")]
        {
            let snapshot_ts = match read_version {
                Some(v) => v.into(),
                None => self.client.current_timestamp().await?,
            };
            let txn = self.client.snapshot(snapshot_ts, Default::default());
            return Ok(Box::new(TikvNativeMvccTxn::new(txn)));
        }
        #[cfg(not(feature = "kv-tikv-native-mvcc"))]
        {
            self.begin_with_generated_version(read_version).await
        }
    }
}
```

The `version` column written to `lance-projection` (renamed from kv-lance — see §6) carries this `u64`, so a graph query running against lance-graph's TiKV provider at snapshot `T` can join against a Lance projection at the same `T`. **One clock, two storage targets.**

---

## 6. Demoting kv-lance → lance-projection

**Goal**: stop using `kv-lance` as a primary backend. Reframe it as a CDC-fed columnar projection of the real KV.

**Why**: from the four-repo design discussion — Lance is the wrong shape for transactional row-MVCC KV (handrolled WAL+memtable+flusher proves the impedance mismatch). It's the right shape for *columnar projection of state owned by another backend*.

### Concrete migration plan

1. **Rename feature** `kv-lance` → `lance-projection` in `core/Cargo.toml`
2. **Move path** `core/src/kvs/lance/` → `core/src/projection/lance/`
3. **Remove the handrolled MVCC layer**:
   - Delete `wal.rs` (the source KV's WAL is canonical)
   - Delete `memtable.rs` (the source KV's memtable is canonical)
   - Delete `flusher.rs`, `commit_gate.rs`, `tx_buffer.rs` (replaced by §6.2 below)
   - Keep `schema.rs` but drop `tombstone` + `version` columns; **use Lance's native versioning** at refresh boundaries
4. **Add CDC consumer** that subscribes to `cf/` events, batches them, writes typed-column Arrow `RecordBatch`es to Lance via `Dataset::append`
5. **Typed columns by default** — read shapes from `lance-graph-catalog` (which sources from the same schema.yml as sea-orm entities)

### CDC consumer sketch (§6.2)

```rust
// core/src/projection/lance/refresher.rs
use arrow_array::RecordBatch;
use lance::Dataset;
use lance_graph_catalog::NodeShape;

pub struct LanceProjectionRefresher {
    source: Arc<dyn Datastore>,         // RocksDB / SurrealKV / TiKV
    target: Dataset,                    // Lance dataset for this projection
    shape: NodeShape,                   // typed schema from catalog
    refresh_interval: Duration,
    last_version: Arc<AtomicU64>,
}

impl LanceProjectionRefresher {
    /// Long-running task: tail cf cursor and append to Lance.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut cursor = self.source.cf_cursor(self.last_version.load(Ordering::Relaxed)).await?;
        loop {
            let batch_events = cursor.next_batch(1000, self.refresh_interval).await?;
            if batch_events.is_empty() { continue; }
            let arrow_batch = self.shape.encode_batch(&batch_events)?;
            self.target.append(arrow_batch, /* params */).await?;
            let new_version = batch_events.last().unwrap().version();
            self.last_version.store(new_version, Ordering::Relaxed);
        }
    }
}
```

### What disappears

- ~90% of `core/src/kvs/lance/` deletes
- Per-row `version: u64` column drops out — Lance manifest versions handle it
- `tombstone: Boolean` drops out — Lance native deletion vectors at refresh time
- Hand-rolled fsync + replay logic deletes

What remains is a CDC tap and an Arrow encoder. **This is the click moment** — kv-lance stops being a peer and becomes the analytical view it should always have been.

---

## 7. Catalog-driven `DEFINE` codegen

`lance-graph-catalog` becomes the source of truth (see lance-graph plan §4). This repo consumes it by adding `surrealdb-cli generate --from-ontology schema.yml` that emits `DEFINE` statements.

Given (single source):

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

### Sprint 0 — `lance-graph-contract` 0.2 consumer
- Pin lance-graph-contract 0.2 once it lands
- Wire the planner IR primitives into surrealdb's planner stub
- E2E: a query the planner can route through lance-graph-contract operators

### Sprint 1 — `surrealdb-ractor` MVP (2 weeks)
- New crate at `surrealdb-ractor/`
- Live query → ractor mailbox routing for one `(query, actor)` pair
- Integration test: a counter actor counts deltas from `LIVE SELECT count(*) FROM events`
- Bench mailbox throughput vs raw cf cursor

### Sprint 2 — `kv-tikv-native-mvcc` (1 week)
- Add feature flag and `TikvNativeMvccTxn`
- Modify `kvs/tikv/Transaction` to honor PD timestamps
- E2E test: a transaction at snapshot `T` reads consistently against a parallel lance-graph-tikv-provider read at same `T`

### Sprint 3 — `lance-projection` (deletes most of `kvs/lance/`) (3 weeks)
- Rename feature + move path
- Delete WAL/memtable/flusher/commit_gate/tx_buffer
- New CDC consumer that subscribes to cf and writes Lance via Arrow append
- Typed columns via catalog
- Migration note in CHANGELOG: breaking for `kv-lance` consumers (none expected — feature was POC)

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
    /// Pre-rubicon — deliberation. Mailbox-local; retractable.
    Deliberate { proposal: serde_json::Value },
    /// Cross the rubicon. Commit the goal to surrealdb.
    Commit,
    /// Awareness — peer goal committed by another actor.
    PeerCommitted(LiveDelta),
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
                state.current_proposal = Some(proposal);  // mailbox-local, no commit
            }
            GoalMsg::Commit => {
                if let Some(p) = state.current_proposal.take() {
                    // Rubicon. The commit emits a cf event;
                    // peers receive PeerCommitted via the router.
                    let _: Vec<serde_json::Value> = state.db
                        .create("goals")
                        .content(p)
                        .await
                        .map_err(|e| ActorProcessingErr::from(anyhow::Error::from(e)))?;
                }
            }
            GoalMsg::PeerCommitted(_delta) => {
                // React to others crossing the rubicon.
                // Drives the "social" coordination of an AGI-style system.
            }
        }
        Ok(())
    }
}
```

### Example 3 — `kv-tikv-native-mvcc` snapshot consistency

```rust
// Read consistently at TiKV HLC timestamp T against:
//   - surrealdb's KV (kv-tikv-native-mvcc)
//   - a Lance projection refreshed up to T
//   - lance-graph-tikv-provider at snapshot T

let snapshot_ts: u64 = pd_client.current_timestamp().await?.into_inner();

let ds = surrealdb::engine::any::connect("tikv://pd:2379").await?;
let kv_result = ds.query("SELECT * FROM users WHERE age > 30")
    .at_version(snapshot_ts)
    .await?;

let provider = TikvNodeTableProvider::new(tikv_client.clone(), shape)
    .await?
    .with_snapshot(snapshot_ts);
let graph_result = CypherQuery::new("MATCH (u:User) WHERE u.age > 30 RETURN u")
    .with_provider("User", Arc::new(provider))
    .execute().await?;

// kv_result and graph_result are consistent at snapshot_ts.
```

---

## 10. Open questions

1. **Crate placement of `surrealdb-ractor`** — top-level here, or under `AdaWorldAPI/ractor-*`? Currently here because cf is internal; revisit if ractor ecosystem grows.
2. **kv-lance demotion is a breaking change** — gate behind `lance-projection-v2` for one cycle, then remove.
3. **Native MVCC fallback** — non-TiKV backends keep local u64 generation. Documented in the feature flag's doc comment.
4. **cf cursor stability** — current cf cursor API isn't versioned. surrealdb-ractor's stability depends on cf surface stability. Coordinate via cf API doc.

---

## 11. Cross-references

- **Glue #2** (TiKV-side TableProvider): `AdaWorldAPI/lance-graph:.claude/plans/integration-plan.md` §5
- **Glue #3** (sea-orm-ractor): `AdaWorldAPI/sea-orm:.claude/plans/integration-plan.md` §5
- **Glue #4** (cognitive-shader-actor): `AdaWorldAPI/lance-graph:.claude/plans/integration-plan.md` §6
- **SIMD kernels**: `AdaWorldAPI/ndarray:.claude/plans/integration-plan.md`
