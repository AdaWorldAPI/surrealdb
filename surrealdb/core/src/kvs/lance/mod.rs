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
mod commit_gate;
mod flusher;
mod memtable;
mod schema;
mod timeline;
mod tx_buffer;
mod wal;

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
use commit_gate::CommitGate;
use flusher::{Flusher, FlusherConfig};
use memtable::{Memtable, Op as MemOp};
use schema::KvSchema;
use tx_buffer::{PendingBuffer, PendingEntry};
use wal::{Wal, WalOp, WalRecord};

use super::Direction;
use super::api::ScanLimit;
use super::config::{LanceConfig, WritePath};
use super::err::{Error, Result};
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

	/// Which write-path `Transaction::commit` takes. Captured at
	/// `Datastore::new` time from [`LanceConfig::write_path`] and
	/// then propagated into each new `Transaction`. The two paths
	/// own disjoint subsets of the per-Datastore state below
	/// (`wal`/`memtable`/`flusher` are LSM-only; `commit_gate` is
	/// LegacyCommitGate-only).
	write_path: WritePath,

	/// CommitGate coordinator. `Some` only when
	/// `write_path == WritePath::LegacyCommitGate`; otherwise the
	/// gate is not spawned and the field stays `None`. Tests that
	/// want to exercise the gate against an LSM-default Datastore
	/// can spawn one directly via [`CommitGate::spawn`] +
	/// [`Datastore::dataset_for_tests`].
	commit_gate: Option<Arc<CommitGate>>,

	/// Write-ahead log for the LSM-style fast-commit path. Writers
	/// append a [`WalRecord`] here (fsynced) before inserting into
	/// the memtable, so a process crash never loses an acknowledged
	/// commit. Replayed once on `Datastore::new`.
	wal: Arc<Wal>,

	/// In-memory write buffer that fronts the Lance dataset. Concurrent
	/// commits land here without blocking on a Lance write; the
	/// background flusher drains the memtable into Lance in batches.
	memtable: Arc<Memtable>,

	/// Background memtable→Lance flusher. Drained on `shutdown`.
	/// `None` when [`LanceConfig::disable_background_flusher`] is set
	/// (test-only durability scenarios — see config docstring).
	flusher: Option<Arc<Flusher>>,

	/// Monotonic per-commit sequence counter. Each committing
	/// transaction fetches one `seq` here (at commit time) and stamps
	/// it on every row it writes, materialised into Lance's `seq`
	/// column. Distinct from the memtable `generation` (which paces
	/// flush boundaries): two commits coalesced into a single Lance
	/// version still carry distinct `seq`s, so per-commit replay is
	/// decoupled from physical batching.
	commit_seq: Arc<AtomicU64>,
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
				// are now the same crate-version as `lance::deps::*`. The
				// `lance::deps::*` indirection used in the lance 1.0.4 era
				// (when our pin was v55 and lance used v56) is no longer
				// necessary.
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
		// any flusher/txn can write. Empty or legacy (pre-`seq`) dataset → 0.
		let seq_floor = Self::max_persisted_seq(&lance_ds).await?;

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

		// Spawn the CommitGate coordinator only when the LegacyCommitGate
		// write-path is selected. The LSM path doesn't use it; spawning
		// would be wasted overhead (an idle tokio task waiting on a
		// submission channel that never receives anything).
		let commit_gate = if config.write_path == WritePath::LegacyCommitGate {
			Some(CommitGate::spawn(Arc::clone(&dataset_arc)))
		} else {
			None
		};

		// Open the LSM write-ahead log and replay any uncommitted
		// entries from a prior crash. The WAL lives inside the Lance
		// dataset's directory; for non-filesystem URIs (e.g. `s3://`)
		// the open will surface a clear "wal mkdir" error — those
		// are not supported by the LSM path.
		let wal_dir = std::path::Path::new(path);
		let wal = Wal::open(wal_dir).await?;
		let replayed = wal.replay().await?;

		// Per-commit sequence counter, seeded ABOVE the maximum `seq`
		// already persisted in Lance (`seq_floor`) so the column stays
		// globally monotonic + unique across restarts (the per-commit
		// replay axis it exists to provide). Asymmetry with `generation`
		// below: `generation` is memtable-local bookkeeping, NOT persisted,
		// so it need only clear the replayed-WAL tail; `seq` IS a Lance
		// column, so re-minting from 0 here would collide with / regress
		// below rows flushed in a prior lifetime (the savant BLOCKER).
		// Replayed WAL records carry no persisted seq (the WAL is keyed on
		// `generation`), so each gets a fresh monotonic seq ABOVE the floor,
		// in WAL order; exact pre-crash seq values are not recovered, but
		// monotonicity + uniqueness are.
		let commit_seq = Arc::new(AtomicU64::new(seq_floor));

		// Build the memtable. Pre-populate from the replayed WAL so
		// that the first read after restart returns the same answers
		// as if the writer had just committed them.
		let memtable = Memtable::new();
		let mut max_replayed_gen: u64 = 0;
		for record in &replayed {
			max_replayed_gen = max_replayed_gen.max(record.generation);
			// One seq per replayed commit (record), shared by its ops.
			let seq = commit_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
			for op in &record.ops {
				match op {
					WalOp::Set { key, val } => memtable.insert_with_seq(
						key.clone(),
						MemOp::Set(val.clone()),
						record.generation,
						seq,
					),
					WalOp::Delete { key } => memtable.insert_with_seq(
						key.clone(),
						MemOp::Delete,
						record.generation,
						seq,
					),
				}
			}
		}
		if !replayed.is_empty() {
			info!(
				target: TARGET,
				"Replayed {} WAL records into memtable (up to gen {max_replayed_gen})",
				replayed.len()
			);
			// Advance the memtable's atomic counter past the highest
			// replayed generation so future commits get strictly
			// monotonic generations across the restart.
			while memtable.current_generation() < max_replayed_gen {
				let _ = memtable.next_generation();
			}
		}

		// Spawn the background memtable→Lance flusher. One per
		// Datastore. The flusher picks up the replayed entries on
		// its first tick and rolls them into Lance, then truncates
		// the WAL.
		//
		// Three conditions skip the spawn:
		// - `write_path == LegacyCommitGate`: the gate path does its
		//   own synchronous Lance commits, so the memtable never
		//   accumulates entries that need flushing.
		// - `disable_background_flusher` (LSM-only test knob): the
		//   recovery tests use this so the WAL is the SOLE durability
		//   source and a `Box::leak` simulated kill cannot race a
		//   mid-flush Lance manifest rewrite.
		let flusher = if config.write_path == WritePath::LegacyCommitGate
			|| config.disable_background_flusher
		{
			None
		} else {
			Some(Flusher::spawn(
				Arc::clone(&dataset_arc),
				Arc::clone(&memtable),
				Arc::clone(&wal),
				FlusherConfig {
					// Tests/ops may widen the periodic tick (None = default).
					tick_interval: config
						.flusher_tick_interval
						.unwrap_or_else(|| FlusherConfig::default().tick_interval),
					..FlusherConfig::default()
				},
			))
		};

		Ok(Datastore {
			dataset: dataset_arc,
			versioned: config.versioned,
			background_optimizer,
			write_path: config.write_path,
			commit_gate,
			wal,
			memtable,
			flusher,
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
			write_path: self.write_path,
			commit_gate: self.commit_gate.clone(),
			wal: Arc::clone(&self.wal),
			memtable: Arc::clone(&self.memtable),
			flusher: self.flusher.clone(),
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
	/// Lets `lance::tests` exercise alternative write paths (notably
	/// the preserved [`CommitGate`] route) directly against the same
	/// Lance handle the production Transaction methods would use,
	/// without exposing the field through any public API. Not
	/// reachable from outside the `lance` module tree.
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
	/// number actually landed in Lance after a flush. Mirrors the
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

	/// Shut down the datastore, flushing any background tasks.
	// Will be called by the kvs::Datastore teardown path in Sprint II+.
	#[allow(dead_code)]
	pub(crate) async fn shutdown(&self) -> Result<()> {
		// Drain the flusher first (if one was spawned — disabled in
		// the recovery test path) so every WAL-acked write lands in
		// Lance before the optimizer stops watching the dataset and
		// the underlying files are released.
		if let Some(flusher) = &self.flusher {
			flusher.shutdown().await;
		}
		// If the LegacyCommitGate write-path was selected, drain its
		// coordinator. `None` on the default LSM path — nothing to do.
		if let Some(gate) = &self.commit_gate {
			gate.shutdown().await;
		}
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
	///
	/// Captured at transaction start so a follow-up commit on the
	/// preserved [`CommitGate`] alternative route (which retains
	/// snapshot-iso semantics, unlike the Sprint AA LSM path) can
	/// pin its scans to the version that was current when the tx
	/// began. The production LSM read path in `get` / `scan_impl`
	/// reads Lance @ latest for unversioned queries, so it doesn't
	/// consult this field — that's why `dead_code` is allowed; the
	/// field stays because the alternative route needs it.
	#[allow(dead_code)]
	read_version: u64,

	/// Shared reference to the underlying Lance dataset.
	dataset: Arc<RwLock<DatasetHandle>>,

	/// Notification hook so commits can wake the optimizer when a
	/// configured write-count threshold is reached.
	background_optimizer: Option<Arc<BackgroundOptimizer>>,

	/// Which write-path this transaction uses for commit/reads.
	/// Copied from the parent Datastore at tx start so we can
	/// dispatch in [`Self::commit`] and the read methods.
	write_path: WritePath,

	/// CommitGate handle. `Some` when `write_path ==
	/// WritePath::LegacyCommitGate`; `None` on the default LSM path.
	commit_gate: Option<Arc<CommitGate>>,

	/// LSM write-ahead log. `commit()` appends to this (fsynced)
	/// before touching the memtable so an acknowledged commit is
	/// always recoverable.
	wal: Arc<Wal>,

	/// In-memory write buffer that fronts the Lance dataset. Reads
	/// check this BEFORE falling through to a Lance scan; writes
	/// land here after the WAL append.
	memtable: Arc<Memtable>,

	/// Handle to the background flusher; commits ping it via
	/// `notify_pending()` so it picks the new entries up promptly
	/// rather than waiting for the next periodic tick. `None` when
	/// `LanceConfig::disable_background_flusher` is set (mirrors
	/// the `Datastore` field).
	flusher: Option<Arc<Flusher>>,

	/// Shared per-commit sequence counter (see the `Datastore` field).
	/// `commit_lsm` fetches one `seq` from here per transaction and
	/// stamps every written row with it.
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

	/// Atomically flush all pending writes/deletes via the configured
	/// write-path. See [`WritePath`] for the semantic differences.
	///
	/// - `WritePath::LsmWithWal` (Sprint AA default): WAL fsync →
	///   memtable insert → notify flusher. Returns Ok as soon as the
	///   WAL append is durable. Lance is updated asynchronously.
	/// - `WritePath::LegacyCommitGate`: submit to the per-Datastore
	///   CommitGate, which batches concurrent submissions into a
	///   single `MergeInsertBuilder` + `delete` against Lance.
	///   Returns only after the Lance commit lands.
	async fn commit(&self) -> Result<()> {
		if self.closed() {
			return Err(Error::TransactionFinished);
		}
		if !self.writeable() {
			return Err(Error::TransactionReadonly);
		}

		// Drain the pending buffer into owned microcopies. After this
		// point the transaction owns the bytes and we drop the read
		// guard before crossing any await boundary.
		let (writes, deletes) = {
			let pending = self.pending.read().await;
			pending.partition()
		};

		if writes.is_empty() && deletes.is_empty() {
			self.done.store(true, Ordering::Release);
			return Ok(());
		}

		match self.write_path {
			WritePath::LsmWithWal | WritePath::LsmColumnar => {
				self.commit_lsm(writes, deletes).await?
			}
			WritePath::LegacyCommitGate => {
				self.commit_legacy_gate(writes, deletes).await?
			}
		}

		self.done.store(true, Ordering::Release);

		// Wake the flusher (when one is spawned — `None` on the
		// LegacyCommitGate path or when explicitly disabled); if the
		// memtable has grown past the threshold it drains on this
		// nudge rather than waiting for the next tick.
		if let Some(flusher) = &self.flusher {
			flusher.notify_pending();
		}

		// Notify the optimizer on both paths — it gauges write activity
		// and may trigger Lance dataset compaction once enough commits
		// have landed.
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

		// (2) Check the memtable — but only on the LSM path.
		//
		// On `LsmWithWal`, committed-but-not-yet-flushed writes live
		// in the memtable; reading them before falling through to
		// Lance is what gives the post-Sprint-AA hot path its speed.
		// On `LegacyCommitGate`, every commit goes directly to Lance,
		// so the memtable is empty and consulting it is dead weight.
		//
		// Versioned reads (`version.is_some()`) skip the memtable on
		// either path — the memtable only holds the latest write per
		// key, not historical versions, so a `get(k, Some(v))` for a
		// past `v` has no business consulting it.
		if version.is_none()
			&& matches!(self.write_path, WritePath::LsmWithWal | WritePath::LsmColumnar)
		{
			if let Some(entry) = self.memtable.get(&key) {
				return Ok(match entry.op {
					MemOp::Set(v) => Some(v),
					MemOp::Delete => None,
				});
			}
		}

		// (3) Fall through to Lance scan.
		//
		// Snapshot selection depends on the write-path:
		//
		// - `LsmWithWal`, `version.is_none()` → read Lance @ LATEST.
		//   The Sprint AA relaxation: the flusher migrates rows from
		//   the memtable into Lance asynchronously, so a tx's
		//   `read_version` snapshot may be stale by the time the
		//   reader actually runs. Pinning to a stale manifest would
		//   hide rows the flusher has just published. Reading Lance
		//   @ latest keeps `memtable[now] ∪ lance[latest]` internally
		//   consistent.
		//
		// - `LegacyCommitGate`, `version.is_none()` → read Lance @
		//   `read_version` for strict snapshot iso. The gate path
		//   never writes to Lance outside of its own commits, so the
		//   manifest at `read_version` is the correct snapshot.
		//
		// - `version.is_some()` → use `checkout_version` on either
		//   path. Caller asked for a specific historical version.
		let ds = self.dataset.read().await;
		let snapshot = match (self.write_path, version) {
			(_, Some(v)) => match ds.inner.checkout_version(v).await {
				Ok(s) => s,
				Err(_) => return Ok(None),
			},
			(WritePath::LsmWithWal | WritePath::LsmColumnar, None) => ds.inner.clone(),
			(WritePath::LegacyCommitGate, None) => {
				match ds.inner.checkout_version(self.read_version).await {
					Ok(s) => s,
					Err(_) => return Ok(None),
				}
			}
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
				// Sprint R unification: direct arrow_array import (same crate
				// + version as lance internally uses, both 57.x).
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
	// ─── Sprint BB: write-path dispatch helpers ──────────────────────────

	/// LSM-style fast commit: append one WAL record (fsync), insert
	/// every op into the memtable, return. Lance is updated later by
	/// the background flusher.
	async fn commit_lsm(
		&self,
		writes: Vec<(Key, Val)>,
		deletes: Vec<Key>,
	) -> Result<()> {
		let generation = self.memtable.next_generation();
		// One per-commit seq for this transaction; every row it writes
		// carries it into Lance's `seq` column. Independent of
		// `generation` so coalesced commits keep distinct seqs.
		let seq = self.commit_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);

		let mut wal_ops = Vec::with_capacity(writes.len() + deletes.len());
		for (k, v) in &writes {
			wal_ops.push(WalOp::Set {
				key: k.clone(),
				val: v.clone(),
			});
		}
		for k in &deletes {
			wal_ops.push(WalOp::Delete {
				key: k.clone(),
			});
		}
		let record = WalRecord {
			generation,
			ops: wal_ops,
		};
		self.wal.append(&record).await?;

		// WAL is durable — now apply to memtable. Order matters:
		// readers should never see a key whose WAL append failed.
		for (k, v) in writes {
			self.memtable.insert_with_seq(k, MemOp::Set(v), generation, seq);
		}
		for k in deletes {
			self.memtable.insert_with_seq(k, MemOp::Delete, generation, seq);
		}
		Ok(())
	}

	/// Legacy CommitGate commit: submit (writes, deletes) to the
	/// per-Datastore coordinator and wait for the synchronous Lance
	/// merge-insert + delete to land. The version stamp is
	/// `read_version + 1` so the row carries a fresh per-row version
	/// monotonically increasing with this transaction's snapshot
	/// boundary.
	async fn commit_legacy_gate(
		&self,
		writes: Vec<(Key, Val)>,
		deletes: Vec<Key>,
	) -> Result<()> {
		let gate = self.commit_gate.as_ref().ok_or_else(|| {
			Error::Datastore(
				"LegacyCommitGate write-path selected but no gate was spawned \
                 on this Datastore — internal invariant violated"
					.into(),
			)
		})?;
		gate.commit(writes, deletes, self.read_version.saturating_add(1))
			.await
	}

	/// Build a `RecordBatch` for the Lance `MergeInsertBuilder` / `Dataset::append`
	/// path. Sprint R unified the arrow type tree: our Cargo.toml pins
	/// `arrow-array = "57"`, which is the same version lance 4.0 uses internally,
	/// so the `lance::deps::arrow_array` indirection from the lance-1.0.4 era
	/// (when our pin was v55 and lance used v56) is no longer necessary.
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

	/// Unified scan/scanr implementation. Merges:
	///   - pending writes (in-memory, overrides Lance)
	///   - Lance dataset state at `read_version`
	///
	/// Then applies limit/skip/direction.
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
		// - LsmWithWal + unversioned → Lance @ latest (Sprint AA
		//   relaxation; see the long comment in `get`).
		// - LegacyCommitGate + unversioned → Lance @ read_version
		//   for strict snapshot iso.
		// - versioned → checkout_version(v) on either path.
		let mut lance_rows: Vec<(Key, Val)> = Vec::new();
		{
			let ds = self.dataset.read().await;
			let snapshot_result: Option<LanceDataset> =
				match (self.write_path, version) {
					(_, Some(v)) => match ds.inner.checkout_version(v).await {
						Ok(s) => Some(s),
						Err(_) => None,
					},
					(WritePath::LsmWithWal | WritePath::LsmColumnar, None) => {
						Some(ds.inner.clone())
					}
					(WritePath::LegacyCommitGate, None) => {
						match ds.inner.checkout_version(self.read_version).await
						{
							Ok(s) => Some(s),
							Err(_) => None,
						}
					}
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
				// lance 1.0.4: Scanner::order_by(Option<Vec<ColumnOrdering>>) -> Result<&mut Self>
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

		// ── (2) Merge with memtable + pending buffer ─────────────────────────
		// Layering, oldest → newest (later layers win on key collision):
		//
		//   Lance     <  memtable  <  pending
		//
		// Versioned reads (`version.is_some()`) skip the memtable: the
		// memtable only holds the latest write per key, not historical
		// versions, so it has no business answering "what did key K
		// look like at version V?" queries.
		{
			let pending = self.pending.read().await;
			let mut merged: std::collections::BTreeMap<Key, Option<Val>> =
				std::collections::BTreeMap::new();
			for (k, v) in lance_rows {
				merged.insert(k, Some(v));
			}
			// Overlay memtable entries within the range — but only on
			// the LSM path. The LegacyCommitGate path never writes to
			// the memtable, so overlaying it would be a no-op anyway,
			// and skipping the iteration saves time on large memtables.
			if version.is_none()
			&& matches!(self.write_path, WritePath::LsmWithWal | WritePath::LsmColumnar)
		{
				for (k, entry) in self.memtable.scan_range(&rng) {
					match entry.op {
						MemOp::Set(v) => {
							merged.insert(k, Some(v));
						}
						MemOp::Delete => {
							merged.insert(k, None);
						}
					}
				}
			}
			// Overlay pending writes: Set overrides everything below,
			// Delete masks the key entirely.
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
			// Materialise in direction order.  BTreeMap iterates ascending by
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration_tests;
