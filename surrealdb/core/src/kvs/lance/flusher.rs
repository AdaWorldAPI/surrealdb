#![cfg(feature = "kv-lance")]

//! Background memtable→Lance flusher for the kv-lance backend.
//!
//! Pairs with [`super::wal::Wal`] and [`super::memtable::Memtable`] to
//! decouple write throughput from Lance's columnar commit latency:
//!
//! ```text
//!   writer → WAL.append (fsync) → Memtable.insert → reply Ok (RAM-fast)
//!   flusher → Memtable.snapshot_up_to(gen) → Lance MergeInsertBuilder
//!           → Memtable.drop_committed → WAL.truncate_to
//! ```
//!
//! ## Trigger conditions
//!
//! The flusher fires on the earliest of:
//!
//! 1. **`tick_interval`** — periodic timer (default 100 ms).
//! 2. **`max_pending_rows`** — once the memtable holds more than this
//!    many entries, the flusher is woken via `trigger()`.
//! 3. **Shutdown** — the [`Self::shutdown`] handle sends a one-shot
//!    signal and the flusher does one final drain before exiting.
//!
//! ## Concurrency contract
//!
//! Only ONE flusher per `Datastore`. Concurrent writers can keep
//! inserting into the memtable while the flusher is mid-commit; the
//! `drop_committed` step uses per-key generation comparison so a
//! racy write is preserved (see `Memtable::drop_committed`
//! documentation).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock, oneshot};
use tracing::{trace, warn};

use super::DatasetHandle;
use super::memtable::{Memtable, Op};
use super::schema::KvSchema;
use super::wal::Wal;
use crate::kvs::err::{Error, Result};
use crate::kvs::{Key, Val};

const TARGET: &str = "surrealdb::core::kvs::lance::flusher";

/// Flusher configuration knobs. All have sensible defaults.
#[derive(Clone, Copy, Debug)]
pub(super) struct FlusherConfig {
	pub tick_interval: Duration,
	pub max_pending_rows: usize,
}

impl Default for FlusherConfig {
	fn default() -> Self {
		Self {
			// 100 ms gives us ~10 flushes/sec under sustained load,
			// which matches the Lance commit cadence observed in
			// `multi_index_concurrent_test_index_compaction`.
			tick_interval: Duration::from_millis(100),
			// 1000 pending rows ≈ ~50-100 KB of memtable. Above this
			// we want to flush regardless of the timer to keep the
			// memtable bounded.
			max_pending_rows: 1000,
		}
	}
}

/// Background memtable→Lance flusher. One per `Datastore`.
pub(super) struct Flusher {
	/// Sends the shutdown signal to the loop. Taken on first
	/// `shutdown()` call.
	shutdown_tx: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
	/// Join handle for the spawned task; awaited on shutdown so the
	/// final drain completes before the caller returns.
	handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
	/// Wake-up hint when the memtable crosses the size threshold or
	/// the caller wants to force-flush.
	notify: Arc<Notify>,
}

impl Flusher {
	/// Spawn the background flusher task.
	pub(super) fn spawn(
		dataset: Arc<RwLock<DatasetHandle>>,
		memtable: Arc<Memtable>,
		wal: Arc<Wal>,
		config: FlusherConfig,
	) -> Arc<Self> {
		let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
		let notify = Arc::new(Notify::new());
		let notify_for_loop = Arc::clone(&notify);

		let handle = tokio::spawn(flusher_loop(
			dataset,
			memtable,
			wal,
			config,
			shutdown_rx,
			notify_for_loop,
		));

		Arc::new(Self {
			shutdown_tx: tokio::sync::Mutex::new(Some(shutdown_tx)),
			handle: tokio::sync::Mutex::new(Some(handle)),
			notify,
		})
	}

	/// Wake the flusher to consider flushing right now.
	/// Called when the memtable crosses a size threshold or when the
	/// caller wants a synchronous flush hint (it does NOT block on
	/// the flush completing).
	pub(super) fn notify_pending(&self) {
		self.notify.notify_one();
	}

	/// Cooperative shutdown — signal the loop, drain a final batch,
	/// `.await` the task. Subsequent calls are no-ops.
	pub(super) async fn shutdown(&self) {
		if let Some(tx) = self.shutdown_tx.lock().await.take() {
			let _ = tx.send(());
		}
		// Wake the loop in case it was sleeping on `tick_interval`.
		self.notify.notify_one();
		if let Some(handle) = self.handle.lock().await.take() {
			let _ = handle.await;
		}
	}
}

async fn flusher_loop(
	dataset: Arc<RwLock<DatasetHandle>>,
	memtable: Arc<Memtable>,
	wal: Arc<Wal>,
	config: FlusherConfig,
	mut shutdown_rx: oneshot::Receiver<()>,
	notify: Arc<Notify>,
) {
	let mut interval = tokio::time::interval(config.tick_interval);
	// Skip the first immediate tick — we want to wait `tick_interval`
	// before the first flush, not flush an empty memtable at startup.
	interval.set_missed_tick_behavior(
		tokio::time::MissedTickBehavior::Delay,
	);
	loop {
		// Wait for: shutdown OR tick OR notify.
		tokio::select! {
			biased;
			_ = &mut shutdown_rx => {
				trace!(target: TARGET, "flusher received shutdown — final drain");
				// Final drain: flush everything up to the current gen.
				let final_gen = memtable.current_generation();
				if let Err(e) = do_flush(&dataset, &memtable, &wal, final_gen).await {
					warn!(target: TARGET, "flusher final drain failed: {e}");
				}
				return;
			}
			_ = interval.tick() => {
				if memtable.is_empty() {
					continue;
				}
			}
			_ = notify.notified() => {
				if memtable.is_empty() {
					continue;
				}
			}
		}

		// Decide whether to flush now: either threshold crossed or
		// the timer fired and we have something to do.
		let pending = memtable.len();
		if pending == 0 {
			continue;
		}
		// Snapshot the current generation as the flush boundary.
		// Any commits that land AFTER this point will be visible via
		// the memtable until the next flush picks them up.
		let snapshot_gen = memtable.current_generation();
		match do_flush(&dataset, &memtable, &wal, snapshot_gen).await {
			Ok(flushed) => {
				trace!(
					target: TARGET,
					"flushed {flushed} entries up to gen {snapshot_gen}"
				);
			}
			Err(e) => {
				warn!(target: TARGET, "flusher cycle failed: {e}");
				// Backoff briefly to avoid hammering the dataset.
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}

		// If the memtable is still over threshold after the flush
		// (e.g. heavy concurrent ingress), loop right away rather
		// than waiting for the next tick.
		if memtable.len() > config.max_pending_rows {
			notify.notify_one();
		}
	}
}

/// Drain everything with `generation <= up_to_gen` into Lance and
/// truncate the WAL. Returns the number of entries flushed.
async fn do_flush(
	dataset: &Arc<RwLock<DatasetHandle>>,
	memtable: &Arc<Memtable>,
	wal: &Arc<Wal>,
	up_to_gen: u64,
) -> Result<usize> {
	// 1. Snapshot the entries (clone — does NOT remove from memtable).
	let snapshot = memtable.snapshot_up_to(up_to_gen);
	if snapshot.is_empty() {
		return Ok(0);
	}

	// 2. Partition into writes and deletes for the Lance commit.
	let mut writes: Vec<(Key, Val)> = Vec::with_capacity(snapshot.len());
	let mut deletes: Vec<Key> = Vec::new();
	let mut flushed_gens: Vec<(Key, u64)> = Vec::with_capacity(snapshot.len());
	for (k, entry) in &snapshot {
		flushed_gens.push((k.clone(), entry.generation));
		match &entry.op {
			Op::Set(v) => writes.push((k.clone(), v.clone())),
			Op::Delete => deletes.push(k.clone()),
		}
	}

	// 3. Commit to Lance — single MergeInsertBuilder + delete pair.
	//    The version stamp is the flush's `up_to_gen`, monotonic
	//    across flushes.
	single_lance_commit(dataset, writes, deletes, up_to_gen).await?;

	// 4. After Lance commit succeeds, drop the flushed entries from
	//    the memtable. Newer-generation writes that raced the flush
	//    are preserved by the per-entry generation comparison inside
	//    `drop_committed`.
	memtable.drop_committed(&flushed_gens);

	// 5. Truncate WAL — keep records with generation > up_to_gen.
	wal.truncate_to(up_to_gen.saturating_add(1)).await?;

	Ok(snapshot.len())
}

/// Issue a single Lance `MergeInsertBuilder::execute_reader` for the
/// writes, followed by `Dataset::delete` for the deletes. One
/// dataset version is produced per call.
///
/// Extracted from the prior `commit_gate::single_lance_commit` —
/// same shape, no longer routed through the gate because the flusher
/// is the only writer to Lance now.
async fn single_lance_commit(
	dataset: &Arc<RwLock<DatasetHandle>>,
	writes: Vec<(Key, Val)>,
	deletes: Vec<Key>,
	version: u64,
) -> Result<()> {
	use lance::dataset::{MergeInsertBuilder, WhenMatched, WhenNotMatched};

	if writes.is_empty() && deletes.is_empty() {
		return Ok(());
	}

	let mut ds = dataset.write().await;

	if !writes.is_empty() {
		let batch = super::Transaction::build_write_batch_lance(&writes, version)
			.map_err(|e| Error::Datastore(format!("lance build batch: {e}")))?;
		let schema_ref = batch.schema();
		let reader = arrow_array::RecordBatchIterator::new(vec![Ok(batch)], schema_ref);

		let arc_ds = Arc::new(ds.inner.clone());
		let (new_ds, _stats) = MergeInsertBuilder::try_new(arc_ds, vec!["key".into()])
			.map_err(|e| Error::Datastore(format!("lance merge builder: {e}")))?
			.when_matched(WhenMatched::UpdateAll)
			.when_not_matched(WhenNotMatched::InsertAll)
			.try_build()
			.map_err(|e| Error::Datastore(format!("lance merge build: {e}")))?
			.execute_reader(reader)
			.await
			.map_err(|e| Error::Datastore(format!("lance merge_insert: {e}")))?;

		ds.inner = Arc::try_unwrap(new_ds).unwrap_or_else(|arc| (*arc).clone());
	}

	if !deletes.is_empty() {
		let predicate = KvSchema::build_delete_predicate(&deletes);
		ds.inner
			.delete(&predicate)
			.await
			.map_err(|e| Error::Datastore(format!("lance delete: {e}")))?;
	}

	Ok(())
}

// End-to-end coverage of the flusher lives in `tests.rs::lsm_*`
// (added when the wire-up in `mod.rs` lands). Pure unit tests of
// `flusher_loop` would require mocking Lance and buy little.
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn flusher_config_defaults_are_sensible() {
		let c = FlusherConfig::default();
		assert!(c.tick_interval >= Duration::from_millis(10));
		assert!(c.max_pending_rows >= 1);
	}
}
