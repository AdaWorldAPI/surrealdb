// =============================================================================
//  PATCH: surrealdb/core/src/kvs/ds.rs
// =============================================================================
//
// Two changes:
//
// (1) Add a `Lance` variant to the `DatastoreFlavor` enum.
//
//     LOCATION: enum DatastoreFlavor, near `SurrealKv` variant.
//
//     BEFORE:
//         pub enum DatastoreFlavor {
//             // ...
//             #[cfg(feature = "kv-surrealkv")]
//             SurrealKv(Arc<super::surrealkv::Datastore>),
//             #[cfg(feature = "kv-tikv")]
//             TiKv(Arc<super::tikv::Datastore>),
//             // ...
//         }
//
//     AFTER:
//         pub enum DatastoreFlavor {
//             // ...
//             #[cfg(feature = "kv-surrealkv")]
//             SurrealKv(Arc<super::surrealkv::Datastore>),
//             #[cfg(feature = "kv-lance")]                                  // ← new
//             Lance(Arc<super::lance::Datastore>),                          // ← new
//             #[cfg(feature = "kv-tikv")]
//             TiKv(Arc<super::tikv::Datastore>),
//             // ...
//         }
//
// (2) Add a `lance:` URL scheme handler in `Datastore::new(url)`.
//
//     LOCATION: match statement in `Datastore::new` (the URL parser).
//
//     EXISTING PATTERN (for surrealkv):
//         #[cfg(feature = "kv-surrealkv")]
//         "surrealkv" => {
//             let path = url.path();
//             let config = SurrealKvConfig::default();
//             let ds = super::surrealkv::Datastore::new(path, config).await?;
//             DatastoreFlavor::SurrealKv(Arc::new(ds))
//         }
//
//     ADD THIS BLOCK:
//         #[cfg(feature = "kv-lance")]
//         "lance" => {
//             let path = url.path();
//             let config = LanceConfig::default();
//             let ds = super::lance::Datastore::new(path, config).await?;
//             DatastoreFlavor::Lance(Arc::new(ds))
//         }
//
// (3) Add `Lance` arm to the `transaction()` dispatch.
//
//     LOCATION: the match statement that maps DatastoreFlavor variants
//     to backend-specific transaction constructors.
//
//     EXISTING PATTERN:
//         #[cfg(feature = "kv-surrealkv")]
//         DatastoreFlavor::SurrealKv(ds) => {
//             let tx = ds.transaction(write, lock).await?;
//             Box::new(tx) as Box<dyn Transactable + Send + Sync>
//         }
//
//     ADD THIS BLOCK:
//         #[cfg(feature = "kv-lance")]
//         DatastoreFlavor::Lance(ds) => {
//             let tx = ds.transaction(write, lock).await?;
//             Box::new(tx) as Box<dyn Transactable + Send + Sync>
//         }
//
// CLI USAGE (after patches applied):
//
//     surreal start lance:///path/to/dataset
//     surreal start lance:///path/to/dataset?versioned=true
//
// Example connection from a client:
//
//     let db = Surreal::new::<Lance>("/path/to/dataset").await?;
