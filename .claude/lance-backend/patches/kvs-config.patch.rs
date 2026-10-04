// =============================================================================
//  PATCH: surrealdb/core/src/kvs/config.rs
// =============================================================================
//
// Add a `LanceConfig` struct that's passed into `Datastore::new`.
// Mirrors the pattern of `SurrealKvConfig`, `RocksDbConfig`.
//
// LOCATION: append to the file, after `SurrealKvConfig`.

/// Configuration for the Lance backend.
///
/// Most knobs live in env-vars (see `kvs/lance/cnf.rs`); this struct
/// only carries the per-Datastore options that the SurrealDB CLI/config
/// layer needs to know about.
#[cfg(feature = "kv-lance")]
#[derive(Debug, Clone)]
pub struct LanceConfig {
	/// Whether to enable per-key versioning (MVCC reads via
	/// `Dataset::checkout(version)`).
	pub versioned: bool,

	/// Whether to write deletions as explicit tombstone rows
	/// (in addition to using Lance's native deletion vectors).
	pub delete_via_tombstone_row: bool,
}

#[cfg(feature = "kv-lance")]
impl Default for LanceConfig {
	fn default() -> Self {
		Self {
			versioned: true,
			delete_via_tombstone_row: false,
		}
	}
}
