# ARCHITECTURE_VISION.md — why kv-lance, why the AdaWorldAPI ndarray fork

> **READ BY:** any session evaluating the kv-lance backend against
> other SurrealDB storage backends (RocksDB, SurrealKv, TiKV, Mem).
> Without this context, kv-lance looks like "another KV backend
> that's slower than RocksDB at OLTP." That framing is wrong.

## The convergence claim

> **One binary. Zero copy. No serialization.**
>
> `surrealdb`, `lance-graph`, and `ndarray` (AdaWorldAPI fork) all
> compile into the same Rust binary and share Arrow buffer pointers
> directly. There is no protobuf wire format between SurrealQL and
> lance-graph; no serde JSON between lance-graph and the HHTL
> cascade; no memcpy from Lance dataset pages to ndarray's SIMD
> `F64x8`. A 16 KB Arrow `BinaryArray` from a Lance scan is the
> SAME memory block that ndarray's `cosine_f64_simd` reads — one
> `&[f64]` slice, no intermediate `Vec`, no serializer round-trip.
>
> This is what "shared substrate" means technically: the storage
> layer (Lance), the query layer (SurrealQL + Cypher / GQL / Gremlin
> / SPARQL via lance-graph), and the compute layer (ndarray SIMD +
> CAM-PQ + HHTL cascade) all access the same Arrow-backed pages
> in-process, by pointer.

```
                  ┌────────────────────────────────────────┐
                  │  ONE BINARY, ZERO COPY, NO SERIALIZE   │
                  │  CPU-only (Pi Zero → Xeon v4 servers)   │
                  └────────────────────────────────────────┘
                                   │
        ┌──────────────────────────┼──────────────────────────┐
        │                          │                          │
        ▼                          ▼                          ▼
 ┌───────────────┐         ┌───────────────┐         ┌──────────────────┐
 │  SurrealDB    │         │  lance-graph  │         │  HHTL cascade    │
 │  5 data       │         │  4+ query     │         │  Heel→Hip→Branch │
 │  modalities   │         │  languages    │         │  →Twig→Leaf      │
 │               │         │               │         │                  │
 │ • document    │         │ • Cypher      │         │ 0.9973 recall    │
 │ • graph       │         │ • GQL         │         │ 16,385^n cand.   │
 │ • relational  │         │ • Gremlin     │         │ no sweep         │
 │ • time-series │         │ • SPARQL      │         │ rolling-σ buckets│
 │ • geospatial  │         │ → SPO         │         │ 256-palette attn │
 └───────┬───────┘         └───────┬───────┘         └────────┬─────────┘
         │                         │                          │
         └─────────────────────────┼──────────────────────────┘
                                   ▼
                  ┌────────────────────────────────────────┐
                  │  Lance columnar datasets (one disk     │
                  │  layout, MVCC, OCC, time-travel,       │
                  │  git-style branching, DataFusion-ready)│
                  └────────────────────────────────────────┘
                                   │
                                   ▼
                  ┌────────────────────────────────────────┐
                  │  ndarray (AdaWorldAPI fork)            │
                  │  SIMD F64x8 polyfill, LazyLock<Tier>   │
                  │  AVX-512 / AVX2 / NEON / scalar        │
                  │  611M cosine ops/sec @ 65W (i7-11700K) │
                  │  CAM-PQ codec, palette ranking, HHTL   │
                  └────────────────────────────────────────┘
```

## The zero-copy path, concretely

A SurrealQL vector-similarity query against a Lance-backed table
flows through these in-process boundaries WITHOUT a serialization
step at any of them:

```
1. SurrealQL parser → AST                       (in-process, owned types)
2. AST → planner → physical plan                (in-process)
3. Planner → kvs::lance::Transaction::scan      (in-process trait call)
4. scan_impl → lance::Dataset::scan().filter()  (in-process; Lance API)
5. Lance returns RecordBatchStream              (Arrow buffers, ref-counted)
6. arrow_array::BinaryArray::value(i)           (&[u8] into the SAME page)
7. SurrealDB vector cosine_distance_f64         (vector-hpc feature on)
8. → ndarray::hpc::heel_f64x8::cosine_f64_simd  (called with the &[f64]
                                                  from step 6 — no copy)
9. F64x8::from_slice(&a[i*8..i*8+8])            (SIMD load from same page)
10. dispatch via LazyLock<Tier> → AVX-512 / AVX2 / NEON / scalar
```

At step 8, the slice handed to `cosine_f64_simd` is a pointer into
the same Arrow buffer that Lance read off disk. No `.to_vec()`, no
`serde_json::to_string`, no `protobuf::Message::write_to_bytes`.
Sprint R (PR #4) deliberately collapsed the dual-arrow type-tree
that would have forced a conversion between `lance::deps::arrow_array`
and our `arrow_array` — those are now the SAME crate at the SAME
version, so the buffer flows through by pointer.

The same applies to lance-graph's HHTL cascade once Phase 3 lands:
the Heel-tier palette buckets read by `blasgraph::PaletteMatrix`
will be the same Arrow column the SurrealQL scan saw, indexed by
the same row offsets. No marshalling, no IPC.

## What kv-lance is NOT competing with

It is **not** a drop-in OLTP replacement for RocksDB or SurrealKv. On
raw single-row write throughput it loses by ~3 orders of magnitude:

| Operation | RocksDB (official) | kv-lance (expected, unmeasured) |
|---|---|---|
| Single `set + commit` | 50–200k ops/sec | 20–100 ops/sec |
| Point `get` (indexed) | 200–500k ops/sec | 1–10k ops/sec |
| Range scan (raw bytes) | 10–50M rows/sec | 100k–1M rows/sec |

If your workload is "lots of small KV puts and direct point lookups
at LSM speed," RocksDB is the right backend. Use `surreal start
rocksdb:///path`.

## What kv-lance IS competing with

It's competing with the **multi-binary, multi-process, GPU-dependent
stack** you'd otherwise need to deliver the same capabilities:

| Need | Conventional stack | This stack |
|---|---|---|
| SurrealQL OLTP + columnar OLAP on same data | SurrealDB + parquet exporter + DuckDB / Spark | **kv-lance** alone |
| Cypher / GQL / Gremlin / SPARQL graph queries | SurrealDB + Neo4j / FalkorDB / Tigergraph | **kv-lance + lance-graph** in-process |
| Vector similarity over millions of candidates | SurrealDB + FAISS / Pinecone / Milvus (GPU) | **kv-lance + ndarray-hpc** CAM-PQ on CPU |
| Time-travel reads (audit, snapshots) | SurrealDB + custom CDC + lakehouse | **kv-lance** native via `Dataset::checkout_version` |
| Branch-per-tenant data isolation | SurrealDB + N database instances | **kv-lance** Lance branches |
| OSINT ontology + OWL DOLCE + CAM-PQ codebook inheritance | Bespoke ETL + triple store + vector store | **kv-lance + lance-graph + ndarray** sharing one substrate |

The competitor isn't a backend, it's a **deployment topology**.

## Performance characteristic that DOES matter

Random access by similarity, not by primary key:

| Operation | Hardware | Throughput | Power |
|---|---|---|---|
| 768-dim cosine via CAM-PQ palette lookup | i7-11700K (consumer) | 2,400M ops/sec | 65W |
| Same on Pi 4 (Cortex-A72) | Pi 4 | ~400M ops/sec | 5W |
| Same on Pi Zero 2W (Cortex-A53) | Pi Zero 2W | ~80M ops/sec | 2W |
| FAISS GPU (IVF-PQ) reference | RTX 3060 | 200–500M ops/sec | 170W |
| FAISS GPU (cuVS) reference | H100 80GB | 1,000–2,000M ops/sec | 700W |

Numbers from `AdaWorldAPI/ndarray/README.md`. The headline:
**611M ops/sec on a 65W consumer CPU beats a 170W GPU and ties a
700W H100 on the same workload**, because the work is a u8 palette
lookup instead of a 768-dim dot product.

PR #2 and #3 wired this for SurrealDB's `idx/trees/vector.rs`
distance functions (cosine, Euclidean, Manhattan, Chebyshev,
Pearson) behind the `vector-hpc` feature flag. The bench at
`surrealdb/core/benches/vector_distance.rs` measures the scalar-vs-SIMD
ratio on the host's CPU.

## HHTL cascade (Heel → Hip → Branch → Twig → Leaf)

The lance-graph search architecture that makes "100M candidates in
one go" tractable on a CPU:

| Tier | Granularity | Filter cost | Survivors after |
|---|---|---|---|
| Heel | coarsest (palette bucket) | O(1) palette compare | ~1% candidates |
| Hip | bucket fan-out | rolling-σ window | ~0.1% |
| Branch | per-doc filter | 256-palette attention rank | ~0.01% |
| Twig | refined similarity | f32 dot + margin | ~0.001% |
| Leaf | exact survivors | full f32 cosine / Euclidean | exact match set |

Recall: **0.9973** over 16,385ⁿ candidates with **no sweep** (no
full-table scan, no candidate pruning that drops the right answer).
Rolling-window σ adjustment adapts to the local density of the
embedding space; 256-palette attention headers provide ranking
without materializing full attention matrices; 10,000×10,000
Gaussian-splat spatial fan-out via blasgraph keeps the working set
in L2/L3 cache.

This is what kv-lance unlocks for SurrealDB: NOT "rocksdb but
columnar" — rather, **SurrealQL queries that route through this
cascade as a built-in operation**, in the same process, without IPC,
without a GPU.

## Operational target

- **Hardware floor:** Pi Zero 2W (Cortex-A53, 1GB RAM, 2W power)
- **Hardware ceiling:** Xeon w9 (Sapphire Rapids, AVX-512) or similar
- **No GPU dependency**, ever — by design
- **No RocksDB binary**, no LSM compaction overhead, no separate KV layer
- **One process**, one Lance dataset path, all queries in-process

## Where this PR series is in the journey

Phase 1 (kv-lance backend, Sprints A–O / PRs #1, #5, #8, #9):
- Days 1–12 of `.claude/lance-backend/DAY_BY_DAY.md`
- Lance 4.0 + arrow 57 + lancedb 0.27 + lance-index 4.0
- BTREE scalar index, atomic MergeInsertBuilder upsert, proper Bytes accounting

Phase 2 (Sprints P–X / PRs #2, #3, #6, #7, #10, #11):
- SIMD distance kernels via F64x8 polyfill (cosine, Euclidean,
  Manhattan, Chebyshev, Pearson)
- x86-64-v3 baseline, `vector-hpc` feature flag
- ndarray fork direct dep, no `[patch.crates-io]`, no ndarray-stats
- Concurrent OCC verified, 42/42 upstream CRUD tests pass on lance

Phase 3 (deferred, this vision document's main subject):
- Wire lance-graph's HHTL cascade as a SurrealQL function
- Expose `blasgraph` GraphBLAS for analytical graph queries
- OWL DOLCE / OSINT / CAM-PQ codebook integration as ontology layer
- Same-binary, same-substrate operational convergence claim becomes
  testable end-to-end

## What this means for "is kv-lance slower than RocksDB"

It is. **And that's not a defect; it's the trade-off that buys
everything in this document.** RocksDB doesn't do columnar OLAP,
doesn't do time-travel, doesn't do branch-per-tenant, doesn't do
CAM-PQ palette ranking at 611M ops/sec, doesn't expose the same
storage layer to a Cypher engine. kv-lance does all of that — at
the cost of being 100–1000× slower at the single-row OLTP path.

For workloads where the operational convergence matters more than
single-row throughput, the math works out heavily in kv-lance's
favor. For OLTP-first workloads, use kv-rocksdb. SurrealDB makes
that a `--features` flag, not an architectural decision.
