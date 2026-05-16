#![cfg(feature = "kv-lance")]

//! Configuration constants for the Lance backend.
//!
//! All values are overridable via `SURREAL_LANCE_*` environment variables,
//! mirroring SurrealKV's `SURREAL_SURREALKV_*` convention.

use std::sync::LazyLock;

/// Whether the background optimizer (compaction + index refresh) is
/// enabled. Default: `true`.
///
/// Disable for unit tests or extreme batch-write workloads where you
/// want to control optimisation timing manually.
pub(super) static LANCE_BACKGROUND_OPTIMIZE_ENABLED: LazyLock<bool> =
	lazy_env_parse!("SURREAL_LANCE_BACKGROUND_OPTIMIZE_ENABLED", bool, true);

/// Minimum number of writes between optimize cycles. Default: 1000.
///
/// When [`Transaction::commit`] flushes pending writes, the optimizer is
/// notified. If the cumulative write-count since the last optimize
/// exceeds this threshold, the next optimize tick will run.
pub(super) static LANCE_OPTIMIZE_AFTER_N_WRITES: LazyLock<u64> =
	lazy_env_parse!("SURREAL_LANCE_OPTIMIZE_AFTER_N_WRITES", u64, 1000);

/// Maximum time (in nanoseconds) between optimize cycles, regardless of
/// write-count. Default: 5 minutes (300_000_000_000 ns).
///
/// Ensures the index doesn't stale-out on a low-write workload.
pub(super) static LANCE_OPTIMIZE_INTERVAL_NS: LazyLock<u64> =
	lazy_env_parse!(duration, "SURREAL_LANCE_OPTIMIZE_INTERVAL", u64, || {
		std::time::Duration::from_secs(300).as_nanos() as u64
	});

/// Maximum age (in seconds) of dataset versions to retain before pruning.
/// Default: 7 days (604800 s). Matches LanceDB's documented default.
///
/// Older versions are deleted by the optimizer to save disk space.
/// Set to 0 to disable version pruning.
pub(super) static LANCE_VERSION_RETENTION_SECS: LazyLock<u64> =
	lazy_env_parse!("SURREAL_LANCE_VERSION_RETENTION_SECS", u64, 7 * 24 * 60 * 60);

/// Whether to write deletions as explicit tombstone rows (`tombstone =
/// true`) in addition to using Lance's native deletion vectors.
/// Default: `false`.
///
/// Tombstone rows make scans see "deleted at version v" as data, which
/// is useful for change-data-capture downstream of SurrealDB. The cost
/// is one extra row per deletion.
// The delete path that reads this flag is deferred to Sprint II+; the
// env-var surface is part of the public config contract.
#[allow(dead_code)]
pub(super) static LANCE_DELETE_VIA_TOMBSTONE_ROW: LazyLock<bool> =
	lazy_env_parse!("SURREAL_LANCE_DELETE_VIA_TOMBSTONE_ROW", bool, false);

/// Whether to create a BTREE scalar index on the `key` column at
/// dataset-open time. Default: `true`.
///
/// Disable when bootstrapping a large dataset where you want to bulk-load
/// first and create the index once afterwards (much faster than
/// incremental index updates per fragment).
pub(super) static LANCE_CREATE_KEY_INDEX_ON_OPEN: LazyLock<bool> =
	lazy_env_parse!("SURREAL_LANCE_CREATE_KEY_INDEX_ON_OPEN", bool, true);

/// Maximum number of rows in a single commit batch before splitting.
/// Default: 10_000.
///
/// Very large transactions create equally large Lance fragments; the
/// committer breaks them into chunks of this size to keep fragments
/// well-shaped for downstream compaction.
// The chunked-commit path is deferred to Sprint II+; the env-var surface
// is part of the public config contract.
#[allow(dead_code)]
pub(super) static LANCE_COMMIT_MAX_BATCH_ROWS: LazyLock<usize> =
	lazy_env_parse!("SURREAL_LANCE_COMMIT_MAX_BATCH_ROWS", usize, 10_000);
