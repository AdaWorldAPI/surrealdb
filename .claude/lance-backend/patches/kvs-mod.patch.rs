// =============================================================================
//  PATCH: surrealdb/core/src/kvs/mod.rs
// =============================================================================
//
// Add `lance` to the storage-backend module list. The pattern mirrors
// `rocksdb`, `surrealkv`, `tikv`, etc.
//
// LOCATION: insert alphabetically between `indxdb` and `mem`.
//
// BEFORE:
//
//     mod indxdb;
//     mod mem;
//     mod rocksdb;
//     mod surrealkv;
//     mod tikv;
//
// AFTER:
//
//     mod indxdb;
//     mod lance;       // ← new
//     mod mem;
//     mod rocksdb;
//     mod surrealkv;
//     mod tikv;
//
// Also extend the top-level doc comment to mention the new backend:
//
//     //! These operations can be processed by the following storage engines:
//     //! - `indxdb`: WASM based database to store data in the browser
//     //! - `lance`: [Lance](https://lance.org) versioned columnar format
//     //!            with native MVCC, OCC, and scalar indexes. Optimised
//     //!            for AI/analytical workloads.
//     //! - `rocksdb`: [RocksDB](...) an embeddable persistent key-value
//     //!   store for fast storage
//     //! - `tikv`: ...
//     //! - `mem`: in-memory database
//     //! - `surrealkv`: ...
