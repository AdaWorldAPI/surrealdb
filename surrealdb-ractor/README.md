# surrealdb-ractor — Glue #1

> **Role:** Bridge SurrealDB live queries / change-feed events into
> [`ractor`](https://crates.io/crates/ractor) actor mailboxes, using
> Apache Arrow `RecordBatch` as the in-process wire format.

## Why this crate exists

The AdaWorldAPI stack targets **zero-copy, single-binary convergence**
across SurrealDB, lance-graph, and ndarray. Part of that vision is
letting actor-based business logic react to database mutations in
real time — without polling and without a serialization layer between
the database and the actors.

`surrealdb-ractor` is the thin glue that makes this possible:

```
SurrealDB LIVE SELECT
        │  change-feed event (JSON / Arrow)
        ▼
 LiveQueryRouter<M>
        │  LiveDelta  (Arrow RecordBatch)
        ▼
 ractor ActorRef<M>  ──▶  actor mailbox
```

## Public API surface (§5 of .claude/plans/integration-plan.md)

| Item | Module | Sprint |
|------|--------|--------|
| `LiveDelta` enum | `delta` | 1 (stub) |
| `LiveDelta::primary_key<T>` | `delta` | 2 |
| `LiveDelta::into_record_batch` | `delta` | 2 |
| `LiveQueryRouter<M>` struct | `router` | 1 (stub) |
| `LiveQueryRouter::run` | `router` | 2 |
| `live_stream(query)` | `stream` | 2 |

All items that list Sprint 1 have `unimplemented!("SD-1 stub — Sprint 1")`
bodies. They compile but panic at runtime. This is intentional — the
scaffold establishes types and module structure before wiring logic.

## Sprint sequence

| Sprint | Worker | Deliverable |
|--------|--------|-------------|
| **1** | SD-1 | This scaffold: `Cargo.toml`, `lib.rs`, `delta.rs`, `router.rs`, `stream.rs` — all stubs |
| **2** | SD-2 | Wire SurrealDB live-query subscription; emit `LiveDelta` from real events |
| **3** | SD-3 | Route `LiveDelta` into ractor mailboxes; back-pressure + error handling |
| **4** | SD-4 | Integration tests + microbench; bump to `0.2.0` |

## Dependencies

| Crate | Role |
|-------|------|
| `surrealdb` (path dep) | Client connection + live-query subscription |
| `ractor` | Actor framework and `ActorRef<M>` mailbox |
| `arrow-array` | `RecordBatch` as the zero-copy data frame |
| `futures` | `Stream` trait for `live_stream` return type |
| `anyhow` | Ergonomic error propagation |
| `tokio` | Async runtime (`rt-multi-thread` + `macros`) |

## cargo check status

Sprint 1 stubs are expected to **compile** (types resolve) but
`cargo check` may emit errors if the `surrealdb` path-dep feature set
is not pinned to a compatible subset — this is acceptable for Sprint 1
and will be resolved in Sprint 2 when the SurrealDB connection type is
used concretely.

## License

BSL 1.1 (→ Apache 2.0 in 2030). See `../LICENSE`.
