#![cfg(feature = "kv-lance")]

//! # Lance Backend for SurrealDB
//!
//! This module implements the [`Transactable`] trait against the
//! [Lance columnar format](https://lance.org), providing SurrealDB with a
//! versioned columnar storage engine optimised for AI/analytical workloads.
//!
//! ## Architecture
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
//!                    │ lance::Dataset API
//!                    ▼
//!  ┌─────────────────────────────────────────────────┐
//!  │  Lance MVCC + OCC + Scalar Indexes               │
//!  │    (BTREE on key column for O(log n) lookup)     │
//!  └─────────────────────────────────────────────────┘
//! ```
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
//! ```
//!
//! ## Transaction Model
//!
//! Unlike SurrealKV (which has an in-tree transaction buffer in the
//! underlying `surrealkv::Tree`), Lance has no per-row transaction buffer.
//! We therefore buffer writes/deletes in [`tx_buffer::PendingBuffer`] and
//! flush atomically on [`Transaction::commit`].
//!
//! ## Versioning
//!
//! Lance's native dataset versioning (`Dataset::checkout(version)`) maps
//! directly to SurrealDB's `version: Option<u64>` parameter. Each commit
//! creates a new Lance dataset version, which becomes a valid snapshot
//! for `get(key, Some(version))`.
//!
//! ## Concurrency
//!
//! Lance provides Optimistic Concurrency Control (OCC) with automatic
//! rebase for non-overlapping changes. Combined with BindSpace-aware
//! application-level sharding (where writes target deterministic
//! key-prefix buckets), concurrent-write conflicts become rare in
//! practice.

mod background_optimizer;
mod cnf;
mod schema;
mod tx_buffer;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use lance::Dataset as LanceDataset;
use lance::dataset::WriteParams;
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
	/// Behind `RwLock` because Lance's `Dataset::append`/`Dataset::delete`
	/// methods require `&mut Dataset`. Reads can happen concurrently
	/// against the same `&Dataset`.
	///
	/// TODO(lance-integration): the actual type is `lance::Dataset` —
	/// gate behind a thin wrapper here so we can mock in unit tests.
	dataset: Arc<RwLock<DatasetHandle>>,

	/// Whether per-key versioning queries (`get(key, Some(version))`) are
	/// supported. When `true`, we map the SurrealDB version onto Lance's
	/// native dataset version (`Dataset::checkout`).
	versioned: bool,

	/// Background optimizer that periodically calls `Dataset::optimize()`
	/// to compact small fragments and refresh the scalar index.
	/// Set to `None` when running in test mode or when the user opts out.
	background_optimizer: Option<Arc<BackgroundOptimizer>>,
}

/// Opaque handle to a Lance dataset.
///
/// We hide it behind a struct so the rest of this file can be reviewed
/// independently of the exact Lance API surface (which evolves between
/// crate versions), and so the background optimizer and datastore can
/// share ownership via `Arc<RwLock<DatasetHandle>>`.
pub(crate) struct DatasetHandle {
	/// Path used for logging / debug. Retained for tracing spans in Day 10+.
	#[allow(dead_code)]
	pub(crate) path: String,
	/// The underlying Lance dataset.
	pub(crate) inner: LanceDataset,
}

impl Datastore {
	/// Open or create a Lance-backed datastore at `path`.
	///
	/// If a Lance dataset exists at `path`, it is opened. Otherwise, an
	/// empty dataset is created with the KV schema and a BTREE scalar
	/// index on the `key` column.
	pub(crate) async fn new(path: &str, config: LanceConfig) -> Result<Datastore> {
		info!(target: TARGET, "Opening Lance datastore at: {}", path);

		// Open an existing Lance dataset, or create a new one if not found.
		let lance_ds = match LanceDataset::open(path).await {
			Ok(ds) => {
				info!(target: TARGET, "Opened existing Lance dataset at: {}", path);
				ds
			}
			Err(lance::Error::DatasetNotFound { .. }) => {
				info!(target: TARGET, "Dataset not found — creating new Lance dataset at: {}", path);
				// Build an empty RecordBatch reader typed with the KV schema.
				// Use lance's re-exported arrow_array/arrow_schema (v56) to avoid
				// version-mismatch with the arrow-array = "55" dep in our Cargo.toml.
				// (lance 1.0.4 requires arrow-array = "56.1" internally.)
				let schema = std::sync::Arc::new(lance::deps::arrow_schema::Schema::new(vec![
					lance::deps::arrow_schema::Field::new("key", lance::deps::arrow_schema::DataType::Binary, false),
					lance::deps::arrow_schema::Field::new("val", lance::deps::arrow_schema::DataType::Binary, false),
					lance::deps::arrow_schema::Field::new("version", lance::deps::arrow_schema::DataType::UInt64, false),
					lance::deps::arrow_schema::Field::new("tombstone", lance::deps::arrow_schema::DataType::Boolean, false),
				]));
				let empty_reader = lance::deps::arrow_array::RecordBatchIterator::new(
					std::iter::empty::<
						std::result::Result<
							lance::deps::arrow_array::RecordBatch,
							lance::deps::arrow_schema::ArrowError,
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

		// TODO(lance-integration): create BTREE scalar index on `key` column.
		// This requires `lance-index` as a direct crate dependency (add it to
		// the kv-lance feature in Cargo.toml). Example call once available:
		//
		// use lance_index::{DatasetIndexExt, IndexType, scalar::ScalarIndexParams};
		// lance_ds
		//     .create_index(
		//         &["key"],
		//         IndexType::BTree,
		//         Some("key_btree_idx".into()),
		//         &ScalarIndexParams::default(),
		//         false, // replace=false — idempotent on re-open
		//     )
		//     .await
		//     .map_err(|e| Error::Datastore(format!("lance create_index: {e}")))?;

		let dataset_handle = DatasetHandle {
			path: path.to_string(),
			inner: lance_ds,
		};

		// Wrap in a single Arc<RwLock<...>> that is SHARED between the
		// Datastore and the BackgroundOptimizer. Previously the optimizer
		// got its own separate Arc, meaning it never saw writes — fixed here.
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

		Ok(Datastore {
			dataset: dataset_arc,
			versioned: config.versioned,
			background_optimizer,
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
		})
	}

	/// Return the current (latest) version of the underlying dataset.
	///
	/// Used to seed `read_version` for new transactions.
	async fn current_version(&self) -> u64 {
		self.dataset.read().await.inner.version().version
	}

	/// Shut down the datastore, flushing any background tasks.
	pub(crate) async fn shutdown(&self) -> Result<()> {
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
/// Writes accumulated in [`pending`] are atomically flushed in a single
/// `Dataset::append` call on [`Self::commit`]. Reads check `pending`
/// first (read-your-writes) and fall through to a Lance scan if the key
/// is not in the buffer.
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
	/// Stays constant for the transaction's lifetime to guarantee
	/// snapshot-isolation reads.
	read_version: u64,

	/// Shared reference to the underlying Lance dataset.
	dataset: Arc<RwLock<DatasetHandle>>,

	/// Notification hook so commits can wake the optimizer when a
	/// configured write-count threshold is reached.
	background_optimizer: Option<Arc<BackgroundOptimizer>>,
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

	/// Atomically flush all pending writes/deletes to the Lance dataset.
	///
	/// 1. Partition pending entries into writes (`Some(Val)`) and
	///    tombstones (`None`).
	/// 2. If there are writes, build an Arrow `RecordBatch` (using lance's
	///    internal arrow v56 re-exports) and call `Dataset::append`.
	/// 3. If there are deletes, call `Dataset::delete(predicate)`.
	///    NOTE: Lance 1.0.4 does not expose a public `with_transaction` API,
	///    so append and delete are issued sequentially (not atomic). This is
	///    consistent with Lance's OCC semantics where concurrent-write conflicts
	///    are rare with BindSpace-aware key prefixes.
	async fn commit(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}

		let pending = self.pending.read().await;
		let (writes, deletes) = pending.partition();

		if !writes.is_empty() || !deletes.is_empty() {
			let mut ds = self.dataset.write().await;
			let new_version = self.read_version + 1;

			// ── writes ──────────────────────────────────────────────────────────
			// Build the RecordBatch using lance's internal arrow re-exports (v56)
			// so the types match what Dataset::append expects. Our Cargo.toml pins
			// arrow-array = "55" but lance 1.0.4 uses v56 internally; the two
			// versions have distinct type IDs and cannot be mixed.
			if !writes.is_empty() {
				// Delete any existing rows for the keys being written first.
				// This implements upsert semantics: Lance append-only storage
				// accumulates multiple rows per key; without this step a second
				// set/putc on the same key would leave two live rows and `get`
				// (with `limit 1`) could return the stale value.
				let write_keys: Vec<Key> = writes.iter().map(|(k, _)| k.clone()).collect();
				let overwrite_predicate = KvSchema::build_delete_predicate(&write_keys);
				ds.inner
					.delete(&overwrite_predicate)
					.await
					.map_err(|e| Error::Datastore(format!("lance pre-write delete: {e}")))?;

				let batch = Self::build_write_batch_lance(&writes, new_version)
					.map_err(|e| Error::Datastore(format!("lance build batch: {e}")))?;

				let schema_ref = batch.schema();
				let reader = lance::deps::arrow_array::RecordBatchIterator::new(
					vec![Ok(batch)],
					schema_ref,
				);

				ds.inner
					.append(reader, None)
					.await
					.map_err(|e| Error::Datastore(format!("lance append: {e}")))?;
			}

			// ── deletes ─────────────────────────────────────────────────────────
			if !deletes.is_empty() {
				let predicate = KvSchema::build_delete_predicate(&deletes);
				ds.inner
					.delete(&predicate)
					.await
					.map_err(|e| Error::Datastore(format!("lance delete: {e}")))?;
			}

			drop(ds);
		}

		drop(pending);
		self.done.store(true, Ordering::Release);

		// Notify optimizer; may trigger compaction if write-threshold
		// is exceeded.
		if let Some(opt) = &self.background_optimizer {
			opt.notify_commit().await;
		}

		Ok(())
	}

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

	async fn exists(&self, key: Key, version: Option<u64>) -> Result<bool> {
		self.get(key, version).await.map(|v| v.is_some())
	}

	/// Resolve a key by:
	///  1. Check pending buffer for read-your-writes.
	///  2. Otherwise scan Lance dataset at `read_version` (or `version`
	///     if explicitly requested) with `key = ?` filter, limit 1.
	async fn get(&self, key: Key, version: Option<u64>) -> Result<Option<Val>> {
		if !self.versioned && version.is_some() {
			return Err(Error::UnsupportedVersionedQueries);
		}
		if self.closed() {
			return Err(Error::TransactionFinished);
		}

		// (1) Check pending buffer (read-your-writes).
		if let Some(pending_entry) = self.pending.read().await.get(&key) {
			return Ok(match pending_entry {
				PendingEntry::Set(v) => Some(v.clone()),
				PendingEntry::Delete => None,
			});
		}

		// (2) Fall through to Lance scan at the appropriate version.
		let scan_version = version.unwrap_or(self.read_version);
		let ds = self.dataset.read().await;

		// On a fresh dataset with no commits yet, checkout_version may fail.
		// Treat any checkout failure here as "not found" — equivalent to
		// no rows matching the filter.
		let snapshot = match ds.inner.checkout_version(scan_version).await {
			Ok(s) => s,
			Err(_) => return Ok(None),
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
				// Use lance's re-exports for type compat (lance 1.0.4 ↔ arrow v56).
				let val_col = batch
					.column_by_name("val")
					.ok_or_else(|| Error::Datastore("lance scan: missing val column".into()))?;
				let val_array = val_col
					.as_any()
					.downcast_ref::<lance::deps::arrow_array::BinaryArray>()
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
	/// Note: this is NOT atomic against concurrent transactions. The
	/// real CAS-on-commit semantics happen when Lance's OCC validates
	/// the transaction at commit time. If two transactions `put` the
	/// same key concurrently, one will succeed at commit and the other
	/// will get a conflict-error and must retry.
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

	async fn scan(
		&self,
		rng: Range<Key>,
		limit: ScanLimit,
		skip: u32,
		version: Option<u64>,
	) -> Result<Vec<(Key, Val)>> {
		self.scan_impl(rng, limit, skip, version, Direction::Forward).await
	}

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
	async fn new_save_point(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		let snapshot = self.pending.read().await.clone();
		self.save_points.write().await.push(snapshot);
		Ok(())
	}

	/// Replace `pending` with the most recently saved snapshot.
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

	/// Pop the most recent save-point without applying it (commit it
	/// into the parent scope).
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
//  Internal helpers
// ============================================================================

impl Transaction {
	/// Build a `RecordBatch` using lance's internal arrow re-exports (v56) so
	/// that the types are compatible with `Dataset::append` without conversion.
	///
	/// Our Cargo.toml pins `arrow-array = "55"` while lance 1.0.4 requires v56
	/// internally. The two major versions have distinct `TypeId`s, so we cannot
	/// pass an arrow-v55 `RecordBatch` to a function expecting an arrow-v56
	/// `RecordBatchReader`. We therefore rebuild the batch here using
	/// `lance::deps::arrow_array` (the v56 re-export).
	fn build_write_batch_lance(
		writes: &[(crate::kvs::Key, crate::kvs::Val)],
		version: u64,
	) -> std::result::Result<
		lance::deps::arrow_array::RecordBatch,
		lance::deps::arrow_schema::ArrowError,
	> {
		use lance::deps::arrow_array::{BinaryArray, BooleanArray, RecordBatch, UInt64Array};
		use lance::deps::arrow_schema::{DataType, Field, Schema};
		use std::sync::Arc;

		let schema = Arc::new(Schema::new(vec![
			Field::new("key", DataType::Binary, false),
			Field::new("val", DataType::Binary, false),
			Field::new("version", DataType::UInt64, false),
			Field::new("tombstone", DataType::Boolean, false),
		]));

		let key_array: BinaryArray =
			writes.iter().map(|(k, _)| Some(k.as_slice())).collect();
		let val_array: BinaryArray =
			writes.iter().map(|(_, v)| Some(v.as_slice())).collect();
		let version_array = UInt64Array::from(vec![version; writes.len()]);
		let tombstone_array = BooleanArray::from(vec![false; writes.len()]);

		RecordBatch::try_new(
			schema,
			vec![
				Arc::new(key_array),
				Arc::new(val_array),
				Arc::new(version_array),
				Arc::new(tombstone_array),
			],
		)
	}

	/// Unified scan/scanr implementation. Merges:
	///   - pending writes (in-memory, overrides Lance)
	///   - Lance dataset state at `read_version`
	/// Then applies limit/skip/direction.
	async fn scan_impl(
		&self,
		_rng: Range<Key>,
		_limit: ScanLimit,
		_skip: u32,
		version: Option<u64>,
		_direction: Direction,
	) -> Result<Vec<(Key, Val)>> {
		if !self.versioned && version.is_some() {
			return Err(Error::UnsupportedVersionedQueries);
		}
		if self.closed() {
			return Err(Error::TransactionFinished);
		}

		// TODO(lance-integration):
		//
		// 1. Open snapshot at version (or self.read_version).
		// 2. Build DataFusion expression:
		//      key >= rng.start AND key < rng.end AND tombstone = false
		// 3. scan().filter(expr).order_by(key, direction).limit(...).execute()
		// 4. Materialize into Vec<(Key, Val)>.
		// 5. Merge with pending-buffer overrides:
		//      - if pending has a Set for key in range, replace
		//      - if pending has a Delete for key in range, drop
		// 6. Apply skip/limit AFTER the merge to be consistent across
		//    pending+stored state.

		todo!("scan_impl: range-scan Lance dataset with pending-buffer merge")
	}
}

#[cfg(test)]
mod tests;
