#![cfg(feature = "kv-lance")]

//! Background optimizer for Lance datasets.
//!
//! Lance separates writes from index/fragment maintenance: writes are
//! cheap (append-only), but the BTREE scalar index and small-fragment
//! compaction need periodic `Dataset::optimize()` calls to stay healthy.
//!
//! This module spawns a single tokio task per [`Datastore`] that wakes
//! either on a configurable interval or on a write-count notification
//! from `Transaction::commit`.
//!
//! ## Trigger conditions
//!
//! The optimize cycle runs when *either* of the following is true:
//!
//! 1. **Time-based**: the configured interval has elapsed since the last
//!    optimize. Default: 5 minutes
//!    ([`cnf::LANCE_OPTIMIZE_INTERVAL_NS`]).
//! 2. **Write-count-based**: the cumulative write-count since the last
//!    optimize exceeds the threshold. Default: 1000 writes
//!    ([`cnf::LANCE_OPTIMIZE_AFTER_N_WRITES`]).
//!
//! Write-count is updated via [`BackgroundOptimizer::notify_commit`],
//! which is cheap (atomic increment) and called inline by `commit()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{Notify, RwLock};

use super::DatasetHandle;

/// A background optimize task that compacts fragments and refreshes the
/// scalar index on the underlying Lance dataset.
pub(super) struct BackgroundOptimizer {
	/// Shared dataset reference.
	#[allow(dead_code)]
	dataset: Arc<RwLock<DatasetHandle>>,

	/// Cumulative write count since the last optimize cycle.
	write_count: AtomicU64,

	/// Threshold for triggering an early optimize.
	write_threshold: u64,

	/// Notifier used by `notify_commit` to wake the optimizer loop
	/// when the threshold is exceeded.
	notify: Arc<Notify>,

	/// Shutdown flag — when set, the background loop exits cleanly.
	shutdown: Arc<AtomicBool>,
}

impl BackgroundOptimizer {
	/// Start the background optimize task. Spawns a tokio task and
	/// returns a handle that can be used to notify it of new writes.
	pub fn start(
		dataset: Arc<RwLock<DatasetHandle>>,
		interval_ns: u64,
		write_threshold: u64,
	) -> Self {
		let notify = Arc::new(Notify::new());
		let shutdown = Arc::new(AtomicBool::new(false));
		let dataset_for_task = Arc::clone(&dataset);
		let notify_for_task = Arc::clone(&notify);
		let shutdown_for_task = Arc::clone(&shutdown);

		tokio::spawn(async move {
			Self::run_loop(
				dataset_for_task,
				notify_for_task,
				shutdown_for_task,
				interval_ns,
			)
			.await;
		});

		Self {
			dataset,
			write_count: AtomicU64::new(0),
			write_threshold,
			notify,
			shutdown,
		}
	}

	/// Increment the write counter and wake the optimizer if the
	/// threshold is exceeded.
	pub async fn notify_commit(&self) {
		let prev = self.write_count.fetch_add(1, Ordering::Relaxed);
		if prev + 1 >= self.write_threshold {
			self.notify.notify_one();
			// Reset the counter optimistically; the optimizer will
			// reset it again on completion. A small race is fine here:
			// at worst we trigger one extra optimize cycle.
			self.write_count.store(0, Ordering::Relaxed);
		}
	}

	/// Shut down the background task gracefully.
	pub async fn shutdown(&self) {
		self.shutdown.store(true, Ordering::Release);
		self.notify.notify_one();
	}

	/// The actual background loop.
	async fn run_loop(
		dataset: Arc<RwLock<DatasetHandle>>,
		notify: Arc<Notify>,
		shutdown: Arc<AtomicBool>,
		interval_ns: u64,
	) {
		let interval = std::time::Duration::from_nanos(interval_ns);
		loop {
			tokio::select! {
				_ = tokio::time::sleep(interval) => {},
				_ = notify.notified() => {},
			}

			if shutdown.load(Ordering::Acquire) {
				break;
			}

			let _ds = dataset.write().await;

			// TODO(lance-integration):
			//
			// Run Lance's optimize pipeline. Combines:
			//   - fragment compaction (merge small fragments)
			//   - scalar-index update (incorporate writes since last
			//     optimize into the BTREE)
			//   - version pruning (delete versions older than retention)
			//
			// ds.inner
			//     .optimize(lance::dataset::optimize::CompactionOptions::default())
			//     .await
			//     .expect("optimize failed");

			// For the POC scaffold we just log that we'd optimize here.
			tracing::debug!(
				target = "surrealdb::core::kvs::lance::optimizer",
				"would call Dataset::optimize() now"
			);
		}
	}
}
