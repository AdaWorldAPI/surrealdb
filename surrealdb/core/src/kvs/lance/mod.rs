#![cfg(feature = "kv-lance")]

//! # Lance Backend for SurrealDB
//!
//! This module implements the [`Transactable`] trait against the
//! [Lance columnar format](https://lance.org), providing SurrealDB with a
//! versioned columnar storage engine optimised for AI/analytical workloads.
//!
//! ## Architecture (native path)
//!
//! ```text
//!  ┌─────────────────────────────────────────────────┐
//!  │  SurrealDB Core (engine, transactions, indexing) │
//!  └─────────────────┬───────────────────────────────┘
//!                    │ Transactable trait
//!                    ▼
//!  ┌─────────────────────────────────────────────────┐
//!  │  kvs/lance/ (this module)                        │
//!  │    Datastore   ── 1 Dataset per Datastore        │
//!  │    Transaction ── pending-buffer + commit-batch  │
//!  └─────────────────┬───────────────────────────────┘
//!                    │ lance::Dataset native API
//!                    ▼
//!  ┌─────────────────────────────────────────────────┐
//!  │  Lance MVCC + OCC + Scalar Indexes               │
//!  │    (BTREE on key column for O(log n) lookup)     │
//!  └─────────────────────────────────────────────────┘
//! ```
//!
//! Unlike the earlier LSM experiment (memtable + WAL + background flusher +
//! commit-gate), this backend reads and writes through lance's **native**
//! path, exactly as `lance-graph` does:
//!
//! - **commit** builds ONE Arrow `RecordBatch` from the transaction's
//!   pending buffer (live rows for writes, tombstone rows for deletes) and
//!   applies it with a single `MergeInsertBuilder::execute_reader`
//!   (`WhenMatched::UpdateAll` / `WhenNotMatched::InsertAll`, keyed on
//!   `key`). One SurrealDB commit = one lance dataset version.
//! - **read** checks the pending buffer first (read-your-writes), then
//!   reads lance (`checkout_version(v)` for an explicit version, else the
//!   latest manifest) with a DataFusion filter + projection, then merges.
//! - **compaction / GC** is lance's own `optimize` via the
//!   [`background_optimizer`], never a hand-rolled flusher.
//!
//! ## Schema
//!
//! Each Datastore is one Lance dataset with the following schema:
//!
//! ```text
//!  key:        Binary       (BTREE scalar index)
//!  val:        Binary
//!  version:    UInt64       (write version for MVCC)
//!  tombstone:  Boolean      (true = key deleted at this version)
//!  seq:        UInt64       (per-commit transaction sequence number)
//! ```
//!
//! ## Transaction Model
//!
//! Lance has no per-row transaction buffer, so we buffer writes/deletes in
//! [`tx_buffer::PendingBuffer`] and apply them atomically as one native
//! merge-insert on [`Transaction::commit`].
//!
//! ## Versioning
//!
//! Lance's native dataset versioning (`Dataset::checkout_version(v)`) maps
//! directly to SurrealDB's `version: Option<u64>` parameter. Each commit
//! creates a new Lance dataset version, which becomes a valid snapshot for
//! `get(key, Some(version))`.
//!
//! ## Concurrency
//!
//! Lance provides Optimistic Concurrency Control (OCC). The merge-insert at
//! commit time is the single point where OCC conflicts surface.

mod background_optimizer;
mod cnf;
mod schema;
mod timeline;
mod tx_buffer;

// `Timeline` is consumed now (the `Datastore::timeline()` return type);
// `TimelineView` + `VersionInfo` are the read-side surface a kanban/replay
// consumer reaches for next. Re-exported crate-wide so that wiring lands
// without churn; `allow(unused_imports)` until the first in-tree consumer.
#[allow(unused_imports)]
pub(crate) use timeline::{Timeline, TimelineView, VersionInfo};

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use lance::Dataset as LanceDataset;
use lance::dataset::WriteParams;
use lance::index::DatasetIndexExt;
use lance_index::IndexType;
use lance_index::scalar::{BuiltinIndexType, ScalarIndexParams};
use tokio::sync::RwLock;

use background_optimizer::BackgroundOptimizer;
use schema::KvSchema;
use tx_buffer::{PendingBuffer, PendingEntry};

use super::Direction;
use super::api::ScanLimit;
use super::config::LanceConfig;
use super::err::{Error, Result};
use crate::key::debug::Sprintable;
use crate::kvs::api::Transactable;
use crate::kvs::{Key, Val};

const TARGET: &str = "surrealdb::core::kvs::lance";

// ============================================================================
//  Datastore
// ============================================================================

/// A SurrealDB datastore backed by a single Lance dataset.
///
/// The Datastore is `Clone`-cheap (all heavy state is behind `Arc`), and
/// multiple Transactions can hold references to it concurrently.
pub struct Datastore {
	/// The Lance dataset that holds all KV pairs.
	///
	/// Behind `RwLock` because the native commit path needs `&mut`/owned
	/// access to swap in the post-merge dataset, while reads clone the
	/// handle and scan a snapshot concurrently.
	dataset: Arc<RwLock<DatasetHandle>>,

	/// Whether per-key versioning queries (`get(key, Some(version))`) are
	/// supported. When `true`, we map the SurrealDB version onto Lance's
	/// native dataset version (`Dataset::checkout_version`).
	versioned: bool,

	/// Background optimizer that periodically calls lance's native
	/// `compact_files` / `cleanup_old_versions` to compact small fragments
	/// and refresh the scalar index. `None` when disabled via config.
	background_optimizer: Option<Arc<BackgroundOptimizer>>,

	/// Monotonic per-commit sequence counter. Each committing transaction
	/// fetches one `seq` here (at commit time) and stamps it on every row
	/// it writes, materialised into Lance's `seq` column. Seeded at open
	/// from [`Self::max_persisted_seq`] so the column stays globally
	/// monotonic + unique across restarts.
	commit_seq: Arc<AtomicU64>,
}

/// Opaque handle to a Lance dataset.
///
/// We hide it behind a struct so the rest of this file can be reviewed
/// independently of the exact Lance API surface (which evolves between
/// crate versions), and so the background optimizer and datastore can
/// share ownership via `Arc<RwLock<DatasetHandle>>`.
pub(crate) struct DatasetHandle {
	/// Path used for logging / debug. Retained for tracing spans.
	#[allow(dead_code)]
	pub(crate) path: String,
	/// The underlying Lance dataset.
	pub(crate) inner: LanceDataset,
}

impl Datastore {
	/// Scan the dataset for the maximum persisted `seq`, so a reopened
	/// `Datastore` seeds its commit-sequence counter ABOVE every seq
	/// already written to Lance — keeping `seq` globally monotonic +
	/// unique across restarts (the per-commit replay axis the column
	/// exists to provide). Returns 0 for an empty dataset, or a legacy
	/// dataset created before the `seq` column existed (a documented
	/// pre-release on-disk migration gap). A future optimization can
	/// read this from manifest metadata instead of a full scan.
	async fn max_persisted_seq(ds: &LanceDataset) -> Result<u64> {
		use futures::TryStreamExt;
		let mut scanner = ds.scan();
		// A dataset whose schema lacks `seq` predates this column (a pre-release
		// on-disk format change). Fail fast with a clear migration error rather
		// than letting the first 5-column merge hit an opaque schema mismatch
		// (codex P2 on #30). A fresh dataset created by this code always carries
		// the 5-column schema, so this only fires for genuinely legacy data.
		if scanner.project(&["seq"]).is_err() {
			return Err(Error::Datastore(
				"Lance dataset predates the `seq` column (pre-release on-disk format change); a backfill/migration is required before writes (see .claude/board/EPIPHANIES.md, 2026-05-30 seq column)."
					.to_string(),
			));
		}
		let mut stream = scanner
			.try_into_stream()
			.await
			.map_err(|e| Error::Datastore(format!("seq seed scan: {e}")))?;
		let mut max_seq: u64 = 0;
		while let Some(batch) = stream
			.try_next()
			.await
			.map_err(|e| Error::Datastore(format!("seq seed next: {e}")))?
		{
			if let Some(col) = batch
				.column_by_name("seq")
				.and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt64Array>())
			{
				for i in 0..col.len() {
					let v = col.value(i);
					if v > max_seq {
						max_seq = v;
					}
				}
			}
		}
		Ok(max_seq)
	}

	/// Open or create a Lance-backed datastore at `path`.
	///
	/// If a Lance dataset exists at `path`, it is opened. Otherwise, an
	/// empty dataset is created with the KV schema and a BTREE scalar
	/// index on the `key` column.
	pub(crate) async fn new(path: &str, config: LanceConfig) -> Result<Datastore> {
		info!(target: TARGET, "Opening Lance datastore at: {}", path);

		// Open an existing Lance dataset, or create a new one if not found.
		let mut lance_ds = match LanceDataset::open(path).await {
			Ok(ds) => {
				info!(target: TARGET, "Opened existing Lance dataset at: {}", path);
				ds
			}
			Err(lance::Error::DatasetNotFound { .. }) => {
				info!(target: TARGET, "Dataset not found — creating new Lance dataset at: {}", path);
				// Build an empty RecordBatch reader typed with the KV schema.
				// Sprint R unification: lance 4.0 and our Cargo.toml both pin
				// arrow-array/schema = "57", so the direct top-level imports
				// are now the same crate-version as `lance::deps::*`.
				let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
					arrow_schema::Field::new("key", arrow_schema::DataType::Binary, false),
					arrow_schema::Field::new("val", arrow_schema::DataType::Binary, false),
					arrow_schema::Field::new("version", arrow_schema::DataType::UInt64, false),
					arrow_schema::Field::new("tombstone", arrow_schema::DataType::Boolean, false),
					arrow_schema::Field::new("seq", arrow_schema::DataType::UInt64, false),
				]));
				let empty_reader = arrow_array::RecordBatchIterator::new(
					std::iter::empty::<
						std::result::Result<
							arrow_array::RecordBatch,
							arrow_schema::ArrowError,
						>,
					>(),
					schema,
				);
				LanceDataset::write(empty_reader, path, Some(WriteParams::default()))
					.await
					.map_err(|e| Error::Datastore(format!("lance create: {e}")))?
			}
			Err(e) => {
				return Err(Error::Datastore(format!("lance open: {e}")));
			}
		};

		// Create a BTREE scalar index on `key` for O(log n) point lookups.
		// Gated on LANCE_CREATE_KEY_INDEX_ON_OPEN so bulk-load scenarios can
		// opt out and build the index once after ingestion (much faster).
		if *cnf::LANCE_CREATE_KEY_INDEX_ON_OPEN {
			let index_params = ScalarIndexParams::for_builtin(BuiltinIndexType::BTree);
			match lance_ds
				.create_index(
					&["key"],
					IndexType::BTree,
					Some("key_btree_idx".into()),
					&index_params,
					false, // replace=false — idempotent on re-open
				)
				.await
			{
				Ok(_) => {
					info!(target: TARGET, "BTREE scalar index on 'key' created/confirmed");
				}
				Err(e) if e.to_string().contains("already exists") => {
					// Index already present — this is the normal case on re-open.
					// Lance returns Err when replace=false and the named index
					// already exists; treat it as success.
				}
				Err(e) => {
					return Err(Error::Datastore(format!("create_index: {e}")));
				}
			}
		}

		// Seed point for the per-commit `seq` counter: the max seq already
		// persisted to Lance. Computed while we still own `lance_ds`, before
		// any txn can write. Empty or legacy (pre-`seq`) dataset → 0.
		let seq_floor = Self::max_persisted_seq(&lance_ds).await?;

		let dataset_handle = DatasetHandle {
			path: path.to_string(),
			inner: lance_ds,
		};

		// Wrap in a single Arc<RwLock<...>> that is SHARED between the
		// Datastore and the BackgroundOptimizer so the optimizer sees the
		// same (post-commit) dataset the transactions mutate.
		let dataset_arc: Arc<RwLock<DatasetHandle>> = Arc::new(RwLock::new(dataset_handle));

		// Spawn background optimizer if enabled, sharing the same Arc.
		let background_optimizer = if *cnf::LANCE_BACKGROUND_OPTIMIZE_ENABLED {
			let opt = BackgroundOptimizer::start(
				Arc::clone(&dataset_arc),
				*cnf::LANCE_OPTIMIZE_INTERVAL_NS,
				*cnf::LANCE_OPTIMIZE_AFTER_N_WRITES,
			);
			Some(Arc::new(opt))
		} else {
			None
		};

		// Per-commit sequence counter, seeded ABOVE the maximum `seq`
		// already persisted in Lance (`seq_floor`) so the column stays
		// globally monotonic + unique across restarts.
		let commit_seq = Arc::new(AtomicU64::new(seq_floor));

		Ok(Datastore {
			dataset: dataset_arc,
			versioned: config.versioned,
			background_optimizer,
			commit_seq,
		})
	}

	/// Begin a new transaction against this datastore.
	pub(crate) async fn transaction(
		&self,
		write: bool,
		_lock: bool,
	) -> Result<Transaction> {
		// Snapshot the current dataset version for read-consistency
		// throughout this transaction.
		let read_version = self.current_version().await;

		Ok(Transaction {
			done: AtomicBool::new(false),
			write,
			versioned: self.versioned,
			pending: Arc::new(RwLock::new(PendingBuffer::new())),
			save_points: Arc::new(RwLock::new(Vec::new())),
			read_version,
			dataset: Arc::clone(&self.dataset),
			background_optimizer: self.background_optimizer.clone(),
			commit_seq: Arc::clone(&self.commit_seq),
		})
	}

	/// Return the current (latest) version of the underlying dataset.
	///
	/// Used to seed `read_version` for new transactions.
	async fn current_version(&self) -> u64 {
		self.dataset.read().await.inner.version().version
	}

	/// Open a read-only [`Timeline`] over this datastore's version history.
	///
	/// This is the "SurrealDB-as-view-over-Lance" surface (the Rubicon
	/// ruling): the timeline enumerates Lance's native dataset versions and
	/// hands out immutable [`TimelineView`]s. It shares the same dataset
	/// handle as live transactions — no second open — and exposes reads
	/// only, so it cannot mutate the leading store.
	pub(crate) fn timeline(&self) -> Timeline {
		Timeline::new(Arc::clone(&self.dataset))
	}

	/// Test-only accessor for the underlying dataset Arc.
	///
	/// Lets `lance::tests` scan the same Lance handle the production
	/// Transaction methods would use, without exposing the field through
	/// any public API. Not reachable from outside the `lance` module tree.
	#[cfg(test)]
	pub(super) fn dataset_for_tests(&self) -> &Arc<RwLock<DatasetHandle>> {
		&self.dataset
	}

	/// Test-only: scan the Lance dataset @ latest for every row's
	/// `(key, version)`. Companion to [`Self::scan_seqs_for_tests`], used to
	/// cross-check the `seq` column against the `version` column.
	#[cfg(test)]
	pub(super) async fn scan_versions_for_tests(&self) -> Result<Vec<(Key, u64)>> {
		use futures::TryStreamExt;
		let ds = self.dataset.read().await;
		let snapshot = ds.inner.clone();
		let mut scanner = snapshot.scan();
		scanner
			.project(&["key", "version"])
			.map_err(|e| Error::Datastore(format!("ver scan project: {e}")))?;
		let mut stream = scanner
			.try_into_stream()
			.await
			.map_err(|e| Error::Datastore(format!("ver scan stream: {e}")))?;
		let mut out: Vec<(Key, u64)> = Vec::new();
		while let Some(batch) = stream
			.try_next()
			.await
			.map_err(|e| Error::Datastore(format!("ver scan next: {e}")))?
		{
			let key_col = batch
				.column_by_name("key")
				.and_then(|c| c.as_any().downcast_ref::<arrow_array::BinaryArray>())
				.ok_or_else(|| Error::Datastore("ver scan: key column".into()))?;
			let ver_col = batch
				.column_by_name("version")
				.and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt64Array>())
				.ok_or_else(|| Error::Datastore("ver scan: version column".into()))?;
			for i in 0..batch.num_rows() {
				out.push((key_col.value(i).to_vec(), ver_col.value(i)));
			}
		}
		Ok(out)
	}

	/// Test-only: scan the Lance dataset @ latest and return every
	/// row's `(key, seq, tombstone)`, regardless of tombstone state.
	///
	/// Used by the `seq`-column tests to assert the per-commit sequence
	/// number actually landed in Lance after a commit. Mirrors the
	/// project/stream idiom of [`Transaction::scan_impl`] but projects
	/// the `key`, `seq`, and `tombstone` columns.
	#[cfg(test)]
	pub(super) async fn scan_seqs_for_tests(&self) -> Result<Vec<(Key, u64, bool)>> {
		use futures::TryStreamExt;

		let ds = self.dataset.read().await;
		let snapshot = ds.inner.clone();
		let mut scanner = snapshot.scan();
		scanner
			.project(&["key", "seq", "tombstone"])
			.map_err(|e| Error::Datastore(format!("seq scan project: {e}")))?;
		let mut stream = scanner
			.try_into_stream()
			.await
			.map_err(|e| Error::Datastore(format!("seq scan stream: {e}")))?;

		let mut out: Vec<(Key, u64, bool)> = Vec::new();
		while let Some(batch) = stream
			.try_next()
			.await
			.map_err(|e| Error::Datastore(format!("seq scan next: {e}")))?
		{
			let key_col = batch
				.column_by_name("key")
				.and_then(|c| c.as_any().downcast_ref::<arrow_array::BinaryArray>())
				.ok_or_else(|| Error::Datastore("seq scan: key column".into()))?;
			let seq_col = batch
				.column_by_name("seq")
				.and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt64Array>())
				.ok_or_else(|| Error::Datastore("seq scan: seq column".into()))?;
			let tomb_col = batch
				.column_by_name("tombstone")
				.and_then(|c| c.as_any().downcast_ref::<arrow_array::BooleanArray>())
				.ok_or_else(|| Error::Datastore("seq scan: tombstone column".into()))?;
			for i in 0..batch.num_rows() {
				out.push((key_col.value(i).to_vec(), seq_col.value(i), tomb_col.value(i)));
			}
		}
		Ok(out)
	}

	/// Shut down the datastore, stopping background tasks.
	// Will be called by the kvs::Datastore teardown path in Sprint II+.
	#[allow(dead_code)]
	pub(crate) async fn shutdown(&self) -> Result<()> {
		// The native path keeps no WAL/memtable/flusher to drain — every
		// commit has already landed in Lance as its own version. Just stop
		// the optimizer so the underlying files are released cleanly.
		if let Some(opt) = &self.background_optimizer {
			opt.shutdown().await;
		}
		Ok(())
	}
}

// ============================================================================
//  Transaction
// ============================================================================

/// A single SurrealDB transaction against a Lance datastore.
///
/// Writes accumulated in [`pending`] are applied as ONE native
/// `MergeInsertBuilder::execute_reader` on [`Self::commit`] (one commit =
/// one lance version). Reads check `pending` first (read-your-writes) and
/// fall through to a Lance scan if the key is not in the buffer.
pub struct Transaction {
	/// Has the transaction been committed or cancelled?
	done: AtomicBool,

	/// Was this transaction created in writeable mode?
	write: bool,

	/// Does the parent datastore support per-key versioning?
	versioned: bool,

	/// Buffered writes/deletes, applied atomically on commit.
	pending: Arc<RwLock<PendingBuffer>>,

	/// Stack of pending-buffer snapshots for nested-transaction support.
	/// `new_save_point()` pushes, `rollback_to_save_point()` pops, etc.
	save_points: Arc<RwLock<Vec<PendingBuffer>>>,

	/// Lance dataset version this transaction reads from.
	///
	/// Captured at transaction start (snapshot isolation). Versioned reads
	/// (`version.is_some()`) use the caller-supplied version instead.
	/// Unversioned reads read Lance @ latest, so this is consulted only as
	/// the base for the per-row `version` stamp written at commit time
	/// (`read_version + 1`); `dead_code` is therefore allowed on the read
	/// side but the field is load-bearing for `commit`. // ///REVIEW: confirm read_version+1 is the intended per-row version stamp vs dataset latest+1
	read_version: u64,

	/// Shared reference to the underlying Lance dataset.
	dataset: Arc<RwLock<DatasetHandle>>,

	/// Notification hook so commits can wake the optimizer when a
	/// configured write-count threshold is reached.
	background_optimizer: Option<Arc<BackgroundOptimizer>>,

	/// Shared per-commit sequence counter (see the `Datastore` field).
	/// `commit` fetches one `seq` from here per transaction and stamps
	/// every written row with it.
	commit_seq: Arc<AtomicU64>,
}

#[async_trait]
impl Transactable for Transaction {
	fn kind(&self) -> &'static str {
		"lance"
	}

	fn closed(&self) -> bool {
		self.done.load(Ordering::Acquire)
	}

	fn writeable(&self) -> bool {
		self.write
	}

	// ------------------------------------------------------------------------
	//  Lifecycle: commit / cancel
	// ------------------------------------------------------------------------

	/// Apply all pending writes/deletes as a SINGLE native lance commit.
	///
	/// Builds one Arrow merge source — live rows for writes
	/// (`tombstone = false`), tombstone rows for deletes (`tombstone =
	/// true`) — and applies both in one `MergeInsertBuilder::execute_reader`
	/// keyed on `key` (`WhenMatched::UpdateAll` / `WhenNotMatched::InsertAll`).
	/// One SurrealDB commit therefore produces exactly one lance dataset
	/// version, and a write+delete commit never leaks a write-before-delete
	/// intermediate version. This is the only place lance OCC conflicts can
	/// surface.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self))]
	async fn commit(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}

		// Drain the pending buffer into owned microcopies. After this point
		// the transaction owns the bytes and we drop the read guard before
		// crossing any await boundary.
		let (writes, deletes) = {
			let pending = self.pending.read().await;
			pending.partition()
		};

		if writes.is_empty() && deletes.is_empty() {
			self.done.store(true, Ordering::Release);
			return Ok(());
		}

		// One per-commit seq for this transaction; every row it writes
		// carries it into Lance's `seq` column.
		let seq = self.commit_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
		// Per-row `version` stamp: the snapshot this txn read from + 1, so
		// each commit's rows carry a version monotonically increasing with
		// its snapshot boundary. The authoritative lance dataset version is
		// assigned by the merge-insert itself; this column is the MVCC
		// convenience stamp the schema documents.
		let version = self.read_version.saturating_add(1); // ///REVIEW: read_version+1 vs dataset-latest+1 — both seen in prior code; pick one consistent with timeline/get expectations
		let write_seqs = vec![seq; writes.len()];
		let delete_seqs = vec![seq; deletes.len()];

		// Build the merge source: live rows for writes, tombstone rows for
		// deletes. Identical schema, so both stream through one reader and
		// land as ONE dataset version.
		let mut batches: Vec<arrow_array::RecordBatch> = Vec::with_capacity(2);
		if !writes.is_empty() {
			batches.push(
				Self::build_write_batch_lance(&writes, version, &write_seqs)
					.map_err(|e| Error::Datastore(format!("lance build batch: {e}")))?,
			);
		}
		if !deletes.is_empty() {
			batches.push(
				Self::build_tombstone_batch_lance(&deletes, version, &delete_seqs)
					.map_err(|e| Error::Datastore(format!("lance build tombstones: {e}")))?,
			);
		}

		// ONE native merge-insert = one lance version.
		Self::execute_merge(&self.dataset, batches).await?;

		self.done.store(true, Ordering::Release);

		// Notify the optimizer — it gauges write activity and may trigger
		// lance compaction once enough commits have landed.
		if let Some(opt) = &self.background_optimizer {
			opt.notify_commit().await;
		}

		Ok(())
	}

	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self))]
	async fn cancel(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		self.pending.write().await.clear();
		self.save_points.write().await.clear();
		self.done.store(true, Ordering::Release);
		Ok(())
	}

	// ------------------------------------------------------------------------
	//  Reads: exists / get
	// ------------------------------------------------------------------------

	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn exists(&self, key: Key, version: Option<u64>) -> Result<bool> {
		self.get(key, version).await.map(|v| v.is_some())
	}

	/// Resolve a key by:
	///  1. Check the pending buffer for read-your-writes.
	///  2. Otherwise scan the Lance dataset — at `version` if explicitly
	///     requested (`checkout_version`), else @ latest — with a
	///     `key = ? AND tombstone = false` filter, limit 1.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn get(&self, key: Key, version: Option<u64>) -> Result<Option<Val>> {
		if !self.versioned && version.is_some() {
			return Err(Error::UnsupportedVersionedQueries);
		}
		if self.closed() {
			return Err(Error::TransactionFinished);
		}

		// (1) Check pending buffer (read-your-writes). A pending tombstone
		// returns None.
		if let Some(pending_entry) = self.pending.read().await.get(&key) {
			return Ok(match pending_entry {
				PendingEntry::Set(v) => Some(v.clone()),
				PendingEntry::Delete => None,
			});
		}

		// (2) Fall through to a native Lance scan.
		//
		// Snapshot selection:
		// - `version.is_some()` → `checkout_version(v)`; a missing/invalid
		//   version yields `None` (`.ok()` keeps this clippy-clean).
		// - `version.is_none()` → read Lance @ latest. Every committed
		//   write is its own lance version, so the latest manifest already
		//   reflects all durable commits; pinning to a stale `read_version`
		//   would hide rows committed by concurrent transactions.
		let ds = self.dataset.read().await;
		let snapshot = match version {
			Some(v) => match ds.inner.checkout_version(v).await.ok() {
				Some(s) => s,
				None => return Ok(None),
			},
			None => ds.inner.clone(),
		};

		let filter = KvSchema::build_get_predicate(&key);

		let mut scanner = snapshot.scan();
		scanner
			.filter(&filter)
			.map_err(|e| Error::Datastore(format!("lance scan filter: {e}")))?
			.project(&["val", "version"])
			.map_err(|e| Error::Datastore(format!("lance scan project: {e}")))?
			.limit(Some(1), None)
			.map_err(|e| Error::Datastore(format!("lance scan limit: {e}")))?;

		let mut stream = scanner
			.try_into_stream()
			.await
			.map_err(|e| Error::Datastore(format!("lance scan stream: {e}")))?;

		use futures::TryStreamExt;
		while let Some(batch) = stream
			.try_next()
			.await
			.map_err(|e| Error::Datastore(format!("lance scan next: {e}")))?
		{
			if batch.num_rows() > 0 {
				let val_col = batch
					.column_by_name("val")
					.ok_or_else(|| Error::Datastore("lance scan: missing val column".into()))?;
				let val_array = val_col
					.as_any()
					.downcast_ref::<arrow_array::BinaryArray>()
					.ok_or_else(|| {
						Error::Datastore("lance scan: val column type mismatch".into())
					})?;
				return Ok(Some(val_array.value(0).to_vec()));
			}
		}

		Ok(None)
	}

	// ------------------------------------------------------------------------
	//  Writes: set / put / putc / del / delc
	// ------------------------------------------------------------------------

	/// Insert or overwrite a key. Buffered until commit.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn set(&self, key: Key, val: Val) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}
		self.pending.write().await.set(key, val);
		Ok(())
	}

	/// Insert only if key does not exist. Performs a read-side check
	/// (`exists`) and then buffers the write.
	///
	/// Note: this is NOT atomic against concurrent transactions. The real
	/// CAS-on-commit semantics happen when Lance's OCC validates the
	/// transaction at commit time. If two transactions `put` the same key
	/// concurrently, one will succeed at commit and the other will get a
	/// conflict-error and must retry.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn put(&self, key: Key, val: Val) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}
		if self.exists(key.clone(), None).await? {
			return Err(Error::TransactionKeyAlreadyExists);
		}
		self.pending.write().await.set(key, val);
		Ok(())
	}

	/// Compare-and-Set: write `val` only if current value matches `chk`.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn putc(&self, key: Key, val: Val, chk: Option<Val>) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}
		let current = self.get(key.clone(), None).await?;
		match (current, chk) {
			(Some(v), Some(w)) if v == w => {
				self.pending.write().await.set(key, val);
				Ok(())
			}
			(None, None) => {
				self.pending.write().await.set(key, val);
				Ok(())
			}
			_ => Err(Error::TransactionConditionNotMet),
		}
	}

	/// Delete a key. Buffered as a tombstone until commit.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn del(&self, key: Key) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}
		self.pending.write().await.delete(key);
		Ok(())
	}

	/// Compare-and-Delete: delete `key` only if current value matches `chk`.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(key = key.sprint()))]
	async fn delc(&self, key: Key, chk: Option<Val>) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}
		let current = self.get(key.clone(), None).await?;
		match (current, chk) {
			(Some(v), Some(w)) if v == w => {
				self.pending.write().await.delete(key);
				Ok(())
			}
			(None, None) => {
				// Nothing to delete; conditional satisfied trivially.
				Ok(())
			}
			_ => Err(Error::TransactionConditionNotMet),
		}
	}

	// ------------------------------------------------------------------------
	//  Range operations: keys / keysr / scan / scanr
	// ------------------------------------------------------------------------

	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(rng = rng.sprint()))]
	async fn keys(
		&self,
		rng: Range<Key>,
		limit: ScanLimit,
		skip: u32,
		version: Option<u64>,
	) -> Result<Vec<Key>> {
		let pairs = self.scan(rng, limit, skip, version).await?;
		Ok(pairs.into_iter().map(|(k, _v)| k).collect())
	}

	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(rng = rng.sprint()))]
	async fn keysr(
		&self,
		rng: Range<Key>,
		limit: ScanLimit,
		skip: u32,
		version: Option<u64>,
	) -> Result<Vec<Key>> {
		let pairs = self.scanr(rng, limit, skip, version).await?;
		Ok(pairs.into_iter().map(|(k, _v)| k).collect())
	}

	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(rng = rng.sprint()))]
	async fn scan(
		&self,
		rng: Range<Key>,
		limit: ScanLimit,
		skip: u32,
		version: Option<u64>,
	) -> Result<Vec<(Key, Val)>> {
		self.scan_impl(rng, limit, skip, version, Direction::Forward).await
	}

	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self), fields(rng = rng.sprint()))]
	async fn scanr(
		&self,
		rng: Range<Key>,
		limit: ScanLimit,
		skip: u32,
		version: Option<u64>,
	) -> Result<Vec<(Key, Val)>> {
		self.scan_impl(rng, limit, skip, version, Direction::Backward).await
	}

	// ------------------------------------------------------------------------
	//  Savepoints (simulated via pending-buffer snapshots)
	// ------------------------------------------------------------------------

	/// Push the current state of `pending` onto the save-point stack.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self))]
	async fn new_save_point(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		let snapshot = self.pending.read().await.clone();
		self.save_points.write().await.push(snapshot);
		Ok(())
	}

	/// Replace `pending` with the most recently saved snapshot.
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self))]
	async fn rollback_to_save_point(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		let snapshot = self
			.save_points
			.write()
			.await
			.pop()
			.ok_or(Error::NoSavePointPresent)?;
		*self.pending.write().await = snapshot;
		Ok(())
	}

	/// Pop the most recent save-point without applying it (commit it into
	/// the parent scope).
	#[instrument(level = "trace", target = "surrealdb::core::kvs::api", skip(self))]
	async fn release_last_save_point(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		self.save_points
			.write()
			.await
			.pop()
			.ok_or(Error::NoSavePointPresent)?;
		Ok(())
	}
}

// ============================================================================
//  Internal helpers (native lance build + merge)
// ============================================================================

impl Transaction {
	/// Build a `RecordBatch` of **live** rows (`tombstone = false`) for the
	/// merge-insert source. Sprint R unified the arrow type tree: our
	/// Cargo.toml pins `arrow-array = "57"`, the same version lance uses
	/// internally, so the `lance::deps::arrow_array` indirection from the
	/// lance-1.0.4 era is no longer necessary.
	pub(super) fn build_write_batch_lance(
		writes: &[(crate::kvs::Key, crate::kvs::Val)],
		version: u64,
		seqs: &[u64],
	) -> std::result::Result<
		arrow_array::RecordBatch,
		arrow_schema::ArrowError,
	> {
		use arrow_array::{BinaryArray, BooleanArray, RecordBatch, UInt64Array};
		use arrow_schema::{DataType, Field, Schema};
		use std::sync::Arc;

		// Enforced in ALL builds (not just debug): a mismatch would otherwise
		// surface only as an opaque Arrow "unequal column length" error.
		if writes.len() != seqs.len() {
			return Err(arrow_schema::ArrowError::InvalidArgumentError(format!(
				"build_write_batch_lance: seqs ({}) must be parallel to writes ({})",
				seqs.len(),
				writes.len()
			)));
		}

		let schema = Arc::new(Schema::new(vec![
			Field::new("key", DataType::Binary, false),
			Field::new("val", DataType::Binary, false),
			Field::new("version", DataType::UInt64, false),
			Field::new("tombstone", DataType::Boolean, false),
			Field::new("seq", DataType::UInt64, false),
		]));

		let key_array: BinaryArray =
			writes.iter().map(|(k, _)| Some(k.as_slice())).collect();
		let val_array: BinaryArray =
			writes.iter().map(|(_, v)| Some(v.as_slice())).collect();
		let version_array = UInt64Array::from(vec![version; writes.len()]);
		let tombstone_array = BooleanArray::from(vec![false; writes.len()]);
		let seq_array = UInt64Array::from(seqs.to_vec());

		RecordBatch::try_new(
			schema,
			vec![
				Arc::new(key_array),
				Arc::new(val_array),
				Arc::new(version_array),
				Arc::new(tombstone_array),
				Arc::new(seq_array),
			],
		)
	}

	/// Build a `RecordBatch` of **tombstone** rows (`tombstone = true`,
	/// empty `val`) for the given keys, stamped at `version`.
	///
	/// Identical Arrow schema to [`Self::build_write_batch_lance`] so a
	/// write batch and a tombstone batch can be streamed into the **same**
	/// `MergeInsertBuilder::execute_reader`. Folding deletions in as
	/// tombstone rows lets a commit that both writes and deletes land as
	/// ONE Lance version instead of a `merge_insert` + `Dataset::delete`
	/// pair, so the version history never exposes a write-before-delete
	/// intermediate that was never an atomic SurrealDB commit. The read
	/// path already hides tombstones (`tombstone = false` in
	/// [`KvSchema::build_get_predicate`] / [`KvSchema::build_range_predicate`]).
	pub(super) fn build_tombstone_batch_lance(
		deletes: &[crate::kvs::Key],
		version: u64,
		seqs: &[u64],
	) -> std::result::Result<arrow_array::RecordBatch, arrow_schema::ArrowError> {
		use arrow_array::{BinaryArray, BooleanArray, RecordBatch, UInt64Array};
		use arrow_schema::{DataType, Field, Schema};
		use std::sync::Arc;

		if deletes.len() != seqs.len() {
			return Err(arrow_schema::ArrowError::InvalidArgumentError(format!(
				"build_tombstone_batch_lance: seqs ({}) must be parallel to deletes ({})",
				seqs.len(),
				deletes.len()
			)));
		}

		let schema = Arc::new(Schema::new(vec![
			Field::new("key", DataType::Binary, false),
			Field::new("val", DataType::Binary, false),
			Field::new("version", DataType::UInt64, false),
			Field::new("tombstone", DataType::Boolean, false),
			Field::new("seq", DataType::UInt64, false),
		]));

		let key_array: BinaryArray = deletes.iter().map(|k| Some(k.as_slice())).collect();
		// Tombstones carry no payload, but `val` is non-nullable, so store an
		// empty byte string. It is never read back: a tombstone row is filtered
		// out by `tombstone = false` before `val` is ever projected.
		let val_array: BinaryArray = deletes.iter().map(|_| Some(&b""[..])).collect();
		let version_array = UInt64Array::from(vec![version; deletes.len()]);
		let tombstone_array = BooleanArray::from(vec![true; deletes.len()]);
		let seq_array = UInt64Array::from(seqs.to_vec());

		RecordBatch::try_new(
			schema,
			vec![
				Arc::new(key_array),
				Arc::new(val_array),
				Arc::new(version_array),
				Arc::new(tombstone_array),
				Arc::new(seq_array),
			],
		)
	}

	/// Apply pre-built merge-source batches to the dataset as a SINGLE
	/// `MergeInsertBuilder::execute_reader` keyed on `key`
	/// (`WhenMatched::UpdateAll` / `WhenNotMatched::InsertAll`). Every batch
	/// must carry the shared KV schema. A commit's pending buffer holds
	/// exactly one op per key, so the merge source has unique keys and the
	/// upsert produces exactly ONE dataset version per commit.
	///
	/// Moved verbatim from the (deleted) `flusher.rs` — this is the proven
	/// native merge call lance-graph also uses.
	async fn execute_merge(
		dataset: &Arc<RwLock<DatasetHandle>>,
		batches: Vec<arrow_array::RecordBatch>,
	) -> Result<()> {
		use lance::dataset::{MergeInsertBuilder, WhenMatched, WhenNotMatched};

		if batches.is_empty() {
			return Ok(());
		}

		let schema_ref = batches[0].schema();
		let reader = arrow_array::RecordBatchIterator::new(
			batches.into_iter().map(Ok::<_, arrow_schema::ArrowError>).collect::<Vec<_>>(),
			schema_ref,
		);

		let mut ds = dataset.write().await;
		let arc_ds = Arc::new(ds.inner.clone());
		// `try_new` takes the join keys; `UpdateAll`/`InsertAll` make this a
		// keyed upsert. `execute_reader` streams the source batches and
		// returns the new dataset handle + merge stats.
		let (new_ds, _stats) = MergeInsertBuilder::try_new(arc_ds, vec!["key".into()])
			.map_err(|e| Error::Datastore(format!("lance merge builder: {e}")))?
			.when_matched(WhenMatched::UpdateAll)
			.when_not_matched(WhenNotMatched::InsertAll)
			.try_build()
			.map_err(|e| Error::Datastore(format!("lance merge build: {e}")))?
			.execute_reader(reader)
			.await
			.map_err(|e| Error::Datastore(format!("lance merge_insert: {e}")))?; // ///REVIEW: lance OCC conflict could be mapped to Error::TransactionRetryable instead of opaque Datastore error (see transactable-contract.md commit())

		ds.inner = Arc::try_unwrap(new_ds).unwrap_or_else(|arc| (*arc).clone());

		Ok(())
	}

	/// Unified scan/scanr implementation. Merges:
	///   - Lance dataset state (at `version` if requested, else @ latest)
	///   - pending writes (in-memory, overrides Lance)
	///
	/// Then applies direction + skip/limit.
	async fn scan_impl(
		&self,
		rng: Range<Key>,
		limit: ScanLimit,
		skip: u32,
		version: Option<u64>,
		direction: Direction,
	) -> Result<Vec<(Key, Val)>> {
		if !self.versioned && version.is_some() {
			return Err(Error::UnsupportedVersionedQueries);
		}
		if self.closed() {
			return Err(Error::TransactionFinished);
		}

		// ── (1) Read Lance rows in range ───────────────────────────────────────
		//
		// Snapshot selection mirrors `Transaction::get`:
		// - `version.is_some()` → `checkout_version(v)` (`.ok()` → None on a
		//   missing/invalid version, which yields an empty Lance side).
		// - `version.is_none()` → Lance @ latest (every commit is its own
		//   version, so latest already reflects all durable commits).
		let mut lance_rows: Vec<(Key, Val)> = Vec::new();
		{
			let ds = self.dataset.read().await;
			let snapshot_result: Option<LanceDataset> = match version {
				Some(v) => ds.inner.checkout_version(v).await.ok(),
				None => Some(ds.inner.clone()),
			};
			if let Some(snapshot) = snapshot_result {
				let filter = KvSchema::build_range_predicate(&rng.start, &rng.end);

				let mut scanner = snapshot.scan();
				scanner
					.filter(&filter)
					.map_err(|e| Error::Datastore(format!("lance scan_impl filter: {e}")))?
					.project(&["key", "val"])
					.map_err(|e| Error::Datastore(format!("lance scan_impl project: {e}")))?;

				// Apply column ordering.
				// lance: Scanner::order_by(Option<Vec<ColumnOrdering>>) -> Result<&mut Self>
				// ColumnOrdering::asc_nulls_first(String) / desc_nulls_first(String) — the
				// ascending flag is baked into the constructor, there is no .with_ascending().
				let ordering = if matches!(direction, Direction::Forward) {
					lance::dataset::scanner::ColumnOrdering::asc_nulls_first("key".to_string())
				} else {
					lance::dataset::scanner::ColumnOrdering::desc_nulls_first("key".to_string())
				};
				scanner
					.order_by(Some(vec![ordering]))
					.map_err(|e| Error::Datastore(format!("lance scan_impl order_by: {e}")))?;

				use futures::TryStreamExt;
				let mut stream = scanner
					.try_into_stream()
					.await
					.map_err(|e| Error::Datastore(format!("lance scan_impl stream: {e}")))?;

				while let Some(batch) = stream
					.try_next()
					.await
					.map_err(|e| Error::Datastore(format!("lance scan_impl next: {e}")))?
				{
					let key_col = batch
						.column_by_name("key")
						.ok_or_else(|| {
							Error::Datastore("lance scan_impl: missing key column".into())
						})?
						.as_any()
						.downcast_ref::<arrow_array::BinaryArray>()
						.ok_or_else(|| {
							Error::Datastore(
								"lance scan_impl: key column type mismatch".into(),
							)
						})?;
					let val_col = batch
						.column_by_name("val")
						.ok_or_else(|| {
							Error::Datastore("lance scan_impl: missing val column".into())
						})?
						.as_any()
						.downcast_ref::<arrow_array::BinaryArray>()
						.ok_or_else(|| {
							Error::Datastore(
								"lance scan_impl: val column type mismatch".into(),
							)
						})?;

					for i in 0..batch.num_rows() {
						lance_rows
							.push((key_col.value(i).to_vec(), val_col.value(i).to_vec()));
					}
				}
			}
		}

		// ── (2) Merge with pending buffer ───────────────────────────────────
		// Layering, oldest → newest (later layers win on key collision):
		//
		//   Lance  <  pending
		//
		// The pending buffer overrides Lance for read-your-writes; a pending
		// Delete masks the key entirely. The merge happens BEFORE skip/limit
		// so ordering is consistent across pending + stored state.
		{
			let pending = self.pending.read().await;
			let mut merged: std::collections::BTreeMap<Key, Option<Val>> =
				std::collections::BTreeMap::new();
			for (k, v) in lance_rows {
				merged.insert(k, Some(v));
			}
			// Overlay pending writes: Set overrides everything below, Delete
			// masks the key entirely.
			for (k, entry) in pending.iter() {
				if k.as_slice() >= rng.start.as_slice()
					&& k.as_slice() < rng.end.as_slice()
				{
					match entry {
						PendingEntry::Set(v) => {
							merged.insert(k.clone(), Some(v.clone()));
						}
						PendingEntry::Delete => {
							merged.insert(k.clone(), None);
						}
					}
				}
			}
			// Materialise in direction order. BTreeMap iterates ascending by
			// default; reverse for Backward.
			let mut combined: Vec<(Key, Val)> = merged
				.into_iter()
				.filter_map(|(k, v)| v.map(|val| (k, val)))
				.collect();
			if matches!(direction, Direction::Backward) {
				combined.reverse();
			}

			// ── (3) Apply skip + limit ─────────────────────────────────────────
			// Three limit kinds (see crate::kvs::api::ScanLimit):
			//   Count(n)             — stop after n entries
			//   Bytes(b)             — stop after key.len()+val.len() ≥ b
			//   BytesOrCount(b, n)   — stop on whichever hits first
			//
			// Per-entry byte cost is key.len() + val.len() (matching the wire-
			// layer accounting used by other backends). The Bytes variant uses
			// "at least n bytes" semantics: we include the first entry that
			// crosses the threshold (so a tiny limit still yields ≥1 row when
			// data exists).
			let skip_n = skip as usize;
			let post_skip = combined.into_iter().skip(skip_n);
			let result: Vec<(Key, Val)> = match limit {
				ScanLimit::Count(n) => post_skip.take(n as usize).collect(),
				ScanLimit::Bytes(b_target) => {
					let b_target = b_target as usize;
					let mut acc = 0usize;
					let mut out = Vec::new();
					for (k, v) in post_skip {
						let cost = k.len() + v.len();
						out.push((k, v));
						acc += cost;
						if acc >= b_target {
							break;
						}
					}
					out
				}
				ScanLimit::BytesOrCount(b_target, n) => {
					let b_target = b_target as usize;
					let n = n as usize;
					let mut acc = 0usize;
					let mut out = Vec::new();
					for (k, v) in post_skip {
						let cost = k.len() + v.len();
						out.push((k, v));
						acc += cost;
						if out.len() >= n || acc >= b_target {
							break;
						}
					}
					out
				}
			};
			Ok(result)
		}
	}
}

// ///REVIEW: tests.rs + integration_tests still reference removed items
// (WritePath, LanceConfig::{write_path,disable_background_flusher,flusher_tick_interval},
// commit_gate module, memtable). These test modules will NOT compile until
// agents 2/3 / the orchestrator update them. Declarations kept as-is per the
// "write only mod.rs" constraint.
#[cfg(test)]
mod tests;


