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
//! The flusher fires on the earliest of (ClickHouse-parity: rows OR
//! bytes OR time), subject to a `min_flush_interval` rate floor:
//!
//! 1. **`tick_interval`** — periodic timer (default 100 ms).
//! 2. **`max_pending_rows`** — once the memtable holds more than this
//!    many entries, the flusher is woken via `notify_pending()`.
//! 3. **`max_pending_bytes`** — once the summed key+val byte size of
//!    the memtable crosses this, the flusher flushes regardless of
//!    row count (large values fill a batch with few rows).
//! 4. **Shutdown** — the [`Self::shutdown`] handle sends a one-shot
//!    signal and the flusher does one final drain before exiting.
//!
//! ## Rate floor ("too many parts" discipline)
//!
//! Each trigger-driven flush is gated by `min_flush_interval` since the
//! last flush. This prevents a burst of small commits from creating
//! Lance versions faster than the background optimizer can compact them
//! (the columnar analogue of ClickHouse's "too many parts"). The
//! periodic `tick_interval` already paces timer flushes; the rate floor
//! additionally caps row/byte-triggered flushes. Shutdown's final drain
//! bypasses the floor — durability wins over batching at teardown.
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
// `web_time::Instant` (not `std::time::Instant`) per the repo WASM rule
// enforced by clippy — see CLAUDE.md "Code Quality Rules".
use web_time::Instant;

use super::DatasetHandle;
use super::memtable::{Memtable, Op};
use super::wal::Wal;
use crate::kvs::err::{Error, Result};
use crate::kvs::{Key, Val};

const TARGET: &str = "surrealdb::core::kvs::lance::flusher";

/// Flusher configuration knobs. All have sensible defaults.
#[derive(Clone, Copy, Debug)]
pub(super) struct FlusherConfig {
	pub tick_interval: Duration,
	pub max_pending_rows: usize,
	/// BYTES trigger: flush once the summed key+val size of the
	/// memtable crosses this, regardless of row count. Catches the
	/// few-large-values case that `max_pending_rows` misses.
	pub max_pending_bytes: usize,
	/// Rate floor: never start a trigger-driven flush sooner than this
	/// since the previous flush. Bounds the rate at which we mint Lance
	/// versions so the background optimizer can keep up ("too many
	/// parts" discipline). The shutdown final drain ignores this.
	pub min_flush_interval: Duration,
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
			// 8 MiB of pending key+val bytes. Bounds memtable RAM for
			// large-value workloads where the row count stays low but
			// each value is big.
			max_pending_bytes: 8 * 1024 * 1024,
			// 50 ms floor between trigger-driven flushes — at most ~20
			// Lance versions/sec from bursts, leaving the optimizer
			// headroom to compact.
			min_flush_interval: Duration::from_millis(50),
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
	// Last time we started a flush, for the `min_flush_interval` rate
	// floor. Seeded one interval in the past so the first trigger isn't
	// needlessly delayed at startup.
	let mut last_flush = Instant::now()
		.checked_sub(config.min_flush_interval)
		.unwrap_or_else(Instant::now);
	loop {
		// Wait for: shutdown OR tick OR notify. `periodic` records
		// whether this wake came from the timer (which flushes any
		// non-empty memtable) vs a `notify_pending()` nudge (which
		// flushes only when a row/byte threshold is crossed).
		let periodic;
		tokio::select! {
			biased;
			_ = &mut shutdown_rx => {
				trace!(target: TARGET, "flusher received shutdown — final drain");
				// Final drain: flush everything up to the current gen.
				// Bypasses the rate floor — durability beats batching
				// at teardown.
				let final_gen = memtable.current_generation();
				if let Err(e) = do_flush(&dataset, &memtable, &wal, final_gen).await {
					warn!(target: TARGET, "flusher final drain failed: {e}");
				}
				return;
			}
			_ = interval.tick() => {
				periodic = true;
			}
			_ = notify.notified() => {
				periodic = false;
			}
		}

		// Decide whether to flush now (rows OR bytes OR periodic-tick),
		// subject to the rate floor. Cheapest measure first: an empty
		// memtable never flushes regardless of why we woke.
		if memtable.is_empty() {
			continue;
		}
		let pending_rows = memtable.len();
		let pending_bytes = memtable.pending_bytes();
		if !should_flush(&config, periodic, pending_rows, pending_bytes, last_flush.elapsed()) {
			// Threshold not crossed, or the rate floor is still in
			// effect. A notify that arrives during the floor window is
			// dropped here; the next periodic tick (or a later notify
			// once the floor has elapsed) will pick the rows up.
			continue;
		}

		// Snapshot the current generation as the flush boundary.
		// Any commits that land AFTER this point will be visible via
		// the memtable until the next flush picks them up.
		let snapshot_gen = memtable.current_generation();
		last_flush = Instant::now();
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

		// If the memtable is still over a threshold after the flush
		// (e.g. heavy concurrent ingress), nudge ourselves to loop
		// again rather than waiting for the next tick. The rate floor
		// still applies on that next iteration.
		if memtable.len() > config.max_pending_rows
			|| memtable.pending_bytes() > config.max_pending_bytes
		{
			notify.notify_one();
		}
	}
}

/// Pure trigger decision for the flusher loop — broken out so it can be
/// unit-tested without spinning up Lance.
///
/// Returns `true` when a flush should start now:
/// - a periodic tick fired (timer paces these; the loop only calls this
///   with a non-empty memtable), OR
/// - the row threshold is crossed (`>= max_pending_rows`), OR
/// - the byte threshold is crossed (`>= max_pending_bytes`),
///
/// but only if `since_last_flush >= min_flush_interval` (the rate floor)
/// — i.e. a trigger never starts a flush sooner than the floor allows.
fn should_flush(
	config: &FlusherConfig,
	periodic: bool,
	pending_rows: usize,
	pending_bytes: usize,
	since_last_flush: Duration,
) -> bool {
	if since_last_flush < config.min_flush_interval {
		return false;
	}
	periodic
		|| pending_rows >= config.max_pending_rows
		|| pending_bytes >= config.max_pending_bytes
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

/// Apply a flush's writes **and** deletes as a SINGLE Lance commit.
///
/// Writes become live rows (`tombstone = false`); deletes become
/// tombstone rows (`tombstone = true`). Both stream into one
/// `MergeInsertBuilder::execute_reader` keyed on `key`. A memtable
/// snapshot holds exactly one op per key, so the merge source has unique
/// keys and the upsert produces exactly ONE dataset version per flush.
///
/// Folding deletes in as tombstone rows (rather than a separate
/// `Dataset::delete`) keeps the Lance version history aligned with flush
/// boundaries: the old `merge_insert` + `Dataset::delete` pair produced
/// *two* versions for a write+delete flush, and the intermediate
/// write-applied/delete-pending version leaked through
/// `Timeline::versions()` as a snapshot that never atomically existed.
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

	// Build the merge source: live rows for writes, tombstone rows for
	// deletes. Identical schema, so both stream through one reader.
	let mut batches: Vec<arrow_array::RecordBatch> = Vec::with_capacity(2);
	if !writes.is_empty() {
		batches.push(
			super::Transaction::build_write_batch_lance(&writes, version)
				.map_err(|e| Error::Datastore(format!("lance build batch: {e}")))?,
		);
	}
	if !deletes.is_empty() {
		batches.push(
			super::Transaction::build_tombstone_batch_lance(&deletes, version)
				.map_err(|e| Error::Datastore(format!("lance build tombstones: {e}")))?,
		);
	}

	let schema_ref = batches[0].schema();
	let reader = arrow_array::RecordBatchIterator::new(
		batches.into_iter().map(Ok::<_, arrow_schema::ArrowError>).collect::<Vec<_>>(),
		schema_ref,
	);

	let mut ds = dataset.write().await;
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
		// Adaptive-batching knobs: a non-trivial byte budget and a
		// sub-tick rate floor that still leaves the optimizer headroom.
		assert!(c.max_pending_bytes >= 64 * 1024);
		assert!(c.min_flush_interval >= Duration::from_millis(1));
		assert!(
			c.min_flush_interval <= c.tick_interval,
			"a periodic tick must never be blocked by the rate floor"
		);
	}

	#[test]
	fn should_flush_triggers_on_rows() {
		let c = FlusherConfig::default();
		let past = c.min_flush_interval; // floor satisfied
		// Below the row threshold and not periodic → no flush.
		assert!(!should_flush(&c, false, c.max_pending_rows - 1, 0, past));
		// At/above the row threshold → flush.
		assert!(should_flush(&c, false, c.max_pending_rows, 0, past));
	}

	#[test]
	fn should_flush_triggers_on_bytes() {
		let c = FlusherConfig::default();
		let past = c.min_flush_interval; // floor satisfied
		// Few rows but the byte budget is crossed → flush on bytes.
		assert!(should_flush(&c, false, 1, c.max_pending_bytes, past));
		assert!(!should_flush(&c, false, 1, c.max_pending_bytes - 1, past));
	}

	#[test]
	fn should_flush_periodic_tick_flushes_nonempty() {
		let c = FlusherConfig::default();
		let past = c.min_flush_interval;
		// A periodic tick flushes even below both thresholds…
		assert!(should_flush(&c, true, 1, 1, past));
		// …but the loop never calls us with an empty memtable, so the
		// rows==0 short-circuit lives in the loop, not here.
	}

	#[test]
	fn should_flush_respects_rate_floor() {
		let c = FlusherConfig::default();
		// Even a periodic tick AND both thresholds crossed must NOT
		// flush if we flushed too recently (rate floor not yet met).
		let too_soon = c.min_flush_interval / 2;
		assert!(!should_flush(&c, true, c.max_pending_rows, c.max_pending_bytes, too_soon));
		assert!(!should_flush(&c, false, c.max_pending_rows, c.max_pending_bytes, too_soon));
		// Exactly at the floor → allowed.
		assert!(should_flush(&c, false, c.max_pending_rows, 0, c.min_flush_interval));
	}
}
