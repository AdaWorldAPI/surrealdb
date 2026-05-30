#![cfg(feature = "kv-lance")]
// The whole module is preserved as an "alternative route" for
// implementation tests (see Sprint AA note in the header below) — its
// public surface is currently exercised ONLY by `tests.rs` via the
// `commit_gate_*` tests. From the production `lib` build, every
// CommitGate item is unused, so `cfg_attr(not(test), allow(dead_code))`
// suppresses the per-item dead-code warnings while keeping the warning
// LIVE for the test build (where a missing call WOULD signal a
// regression).
#![cfg_attr(not(test), allow(dead_code))]

//! # Commit coordinator — CollapseGate / BUNDLE merge for concurrent commits
//!
//! > **Sprint AA note (2026-05):** the production Transaction commit path
//! > now goes through the WAL + memtable + background-flusher pipeline in
//! > [`super::wal`], [`super::memtable`], and [`super::flusher`]. The
//! > CommitGate below is **preserved as an alternative route** that
//! > implementation tests can spawn directly via [`CommitGate::spawn`]
//! > to compare LSM-style hot-path behaviour against the older
//! > synchronous-batched-commit semantics, or to drive workloads that
//! > prefer strict snapshot isolation over throughput. It is NOT
//! > currently invoked by [`Transaction::commit`], so the per-Datastore
//! > coordinator task is effectively idle in production — that idleness
//! > is intentional and the file must not be deleted.
//!
//! Multiple in-flight Transactions calling [`Transaction::commit`] concurrently
//! would otherwise each issue their own `MergeInsertBuilder::execute_reader`
//! against the shared Lance dataset, producing N independent dataset versions
//! and triggering Lance's OCC retry path. Under high contention (e.g. the
//! upstream `multi_index_concurrent_test_index_compaction` test, which spawns
//! 54 concurrent writers + a background `index_compaction` loop) the retry
//! cascade exhausts the wall-clock budget.
//!
//! This module implements the lance-graph CollapseGate write protocol from
//! `ndarray::hpc::data-flow.md`:
//!
//! - **BindSpace read-only zero-copy** = each Transaction's `read_version`
//!   snapshot (immutable view of the dataset).
//! - **Owned microcopies for write scope** = each Transaction's
//!   `PendingBuffer` (heap-local writes/deletes; not visible to readers).
//! - **CollapseGate** (this module) = a single per-Datastore coordinator
//!   that batches N concurrent submissions inside a small time/size window
//!   and issues **ONE** `MergeInsertBuilder::execute_reader` per batch,
//!   collapsing N would-be commits into 1.
//!
//! The merge semantics are [`MergeMode::Bundle`] from
//! `lance_graph_contract::collapse_gate`: when two submissions in the same
//! batch touch the same key, the later submitter's value wins. This matches
//! upstream SurrealDB's serial-commit semantics where two transactions
//! writing the same key see "the last commit wins" — only here we coalesce
//! the work into a single Lance commit.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, mpsc, oneshot};
use tracing::{trace, warn};

use super::DatasetHandle;
use crate::kvs::err::{Error, Result};
use crate::kvs::{Key, Val};

const TARGET: &str = "surrealdb::core::kvs::lance::commit_gate";

/// Batch window — coordinator waits at most this long after the first
/// submission before flushing the batch.
///
/// 500 µs is short enough that single-writer latency stays under 1 ms (the
/// per-commit Lance round-trip on local disk is already several ms, so the
/// 500 µs wait is dwarfed by the commit itself), and long enough that bursts
/// of concurrent commits naturally coalesce.
const BATCH_WINDOW: Duration = Duration::from_micros(500);

/// Hard cap on submissions per batch. Above this, the gate flushes
/// regardless of the window timer.
const MAX_BATCH_SIZE: usize = 64;

/// Bounded channel for backpressure. If the channel fills up, new submitters
/// `.await` on `send` until the coordinator drains.
const CHANNEL_CAPACITY: usize = 256;

/// A single in-flight Transaction's commit request.
struct Submission {
	writes: Vec<(Key, Val)>,
	deletes: Vec<Key>,
	/// Version to stamp on every written row. The gate uses `max(version)`
	/// across the batch as the canonical stamp for all rows in the merged
	/// RecordBatch — monotonicity is preserved since each transaction's
	/// `version = read_version + 1` is at least as large as its base.
	version: u64,
	reply: oneshot::Sender<BatchOutcome>,
}

/// Compact, `Clone` outcome that the coordinator broadcasts to every
/// submitter in a batch. `Error` itself does not impl `Clone`, so we collapse
/// to the two categories that callers need to distinguish (`TransactionConflict`
/// gates SurrealDB's retry loop via `Error::is_retryable`; everything else
/// surfaces as a generic datastore failure).
#[derive(Clone, Debug)]
enum BatchOutcome {
	Ok,
	Conflict(String),
	Datastore(String),
}

impl BatchOutcome {
	fn from_err(e: &Error) -> Self {
		match e {
			Error::TransactionConflict(s) => Self::Conflict(s.clone()),
			other => Self::Datastore(other.to_string()),
		}
	}

	fn into_result(self) -> Result<()> {
		match self {
			Self::Ok => Ok(()),
			Self::Conflict(s) => Err(Error::TransactionConflict(s)),
			Self::Datastore(s) => Err(Error::Datastore(s)),
		}
	}
}

/// Per-Datastore commit coordinator. Held inside an `Arc` and shared with
/// every in-flight Transaction via `Transaction::commit_gate`.
pub(super) struct CommitGate {
	submit: mpsc::Sender<Submission>,
	/// Held to signal the coordinator task to drain & exit. `None` after
	/// `shutdown` has been invoked.
	shutdown: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
	/// Join handle for the coordinator task, so [`Self::shutdown`] can
	/// `.await` it and guarantee that all queued submissions have been
	/// drained and acknowledged before returning.
	coordinator: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CommitGate {
	/// Spawn the coordinator task. The returned `Arc<Self>` is cloned into
	/// each Transaction.
	pub(super) fn spawn(dataset: Arc<RwLock<DatasetHandle>>) -> Arc<Self> {
		let (submit_tx, submit_rx) = mpsc::channel::<Submission>(CHANNEL_CAPACITY);
		let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

		let coordinator = tokio::spawn(coordinator_loop(dataset, submit_rx, shutdown_rx));

		Arc::new(Self {
			submit: submit_tx,
			shutdown: tokio::sync::Mutex::new(Some(shutdown_tx)),
			coordinator: tokio::sync::Mutex::new(Some(coordinator)),
		})
	}

	/// Submit a batch of (writes, deletes) and wait for the coordinator's
	/// gated commit to land.
	pub(super) async fn commit(
		&self,
		writes: Vec<(Key, Val)>,
		deletes: Vec<Key>,
		version: u64,
	) -> Result<()> {
		// Empty submissions short-circuit without touching the gate.
		if writes.is_empty() && deletes.is_empty() {
			return Ok(());
		}
		let (reply_tx, reply_rx) = oneshot::channel();
		self.submit
			.send(Submission {
				writes,
				deletes,
				version,
				reply: reply_tx,
			})
			.await
			.map_err(|_| Error::Datastore("commit gate coordinator shut down".into()))?;
		reply_rx
			.await
			.map_err(|_| Error::Datastore("commit gate coordinator dropped reply".into()))?
			.into_result()
	}

	/// Cooperative shutdown — signal the coordinator to drain pending work
	/// and exit, then `.await` the coordinator task so all queued
	/// submissions have been committed and replied to before this returns.
	///
	/// Subsequent calls are no-ops (both the signal channel and the join
	/// handle are taken on first call).
	pub(super) async fn shutdown(&self) {
		// 1. Fire the shutdown signal so the coordinator transitions
		//    into its drain phase (closes the submit channel, then keeps
		//    flushing batches until the channel is empty).
		if let Some(tx) = self.shutdown.lock().await.take() {
			let _ = tx.send(());
		}
		// 2. Wait for the coordinator task to actually exit. Without
		//    this, callers of `Datastore::shutdown()` race against the
		//    coordinator and may observe a dropped-reply error on
		//    commits that were enqueued just before the shutdown signal.
		if let Some(handle) = self.coordinator.lock().await.take() {
			let _ = handle.await;
		}
	}
}

/// The coordinator's main loop. Owns the receiver end of the submission
/// channel and the dataset write-handle.
///
/// On shutdown signal, the loop transitions into a drain phase: the
/// submission channel is closed (no new submitters can enqueue) and any
/// already-queued submissions are flushed as final batches before the
/// coordinator returns. Without this, in-flight `commit()` callers
/// would receive a "coordinator dropped reply" error even though
/// `Datastore::shutdown` is documented to drain the gate first.
async fn coordinator_loop(
	dataset: Arc<RwLock<DatasetHandle>>,
	mut submit_rx: mpsc::Receiver<Submission>,
	mut shutdown_rx: oneshot::Receiver<()>,
) {
	let mut shutting_down = false;
	loop {
		// Wait for the first submission. While not shutting down, also
		// race against the shutdown signal so we can transition into the
		// drain phase. After shutdown is signalled we only `recv` — the
		// channel is closed, so `recv` returns `None` once the queue is
		// empty and we exit cleanly.
		let first = if shutting_down {
			match submit_rx.recv().await {
				Some(s) => s,
				None => {
					trace!(
						target: TARGET,
						"commit gate drained and exiting after shutdown"
					);
					return;
				}
			}
		} else {
			tokio::select! {
				biased;
				_ = &mut shutdown_rx => {
					trace!(
						target: TARGET,
						"commit gate received shutdown signal — draining queued submissions"
					);
					// Stop accepting new submissions; the channel
					// stays open for already-queued items to drain.
					submit_rx.close();
					shutting_down = true;
					continue;
				},
				sub = submit_rx.recv() => match sub {
					Some(s) => s,
					None => {
						trace!(target: TARGET, "commit gate submission channel closed");
						return;
					},
				},
			}
		};

		// Open the batch window — accumulate until MAX_BATCH_SIZE or BATCH_WINDOW.
		// During drain (shutting_down), we still respect the window but the
		// `recv()` calls will return `None` as soon as the queue empties, so
		// we naturally tail off into a final batch rather than waiting the
		// full 500 µs for nothing.
		let mut batch = Vec::with_capacity(MAX_BATCH_SIZE);
		batch.push(first);
		let deadline = tokio::time::Instant::now() + BATCH_WINDOW;
		while batch.len() < MAX_BATCH_SIZE {
			match tokio::time::timeout_at(deadline, submit_rx.recv()).await {
				Ok(Some(sub)) => batch.push(sub),
				Ok(None) => break,
				Err(_) => break, // window expired
			}
		}

		let batch_len = batch.len();
		trace!(
			target: TARGET,
			"commit gate flushing batch of {} submissions (shutting_down={})",
			batch_len,
			shutting_down
		);

		// Execute the coalesced commit and broadcast the result.
		execute_batch(&dataset, batch).await;
	}
}

/// Coalesce all submissions in `batch` by key (BUNDLE semantics: last-submit
/// wins), issue ONE Lance commit, and broadcast the outcome to every
/// submitter's reply channel.
async fn execute_batch(dataset: &Arc<RwLock<DatasetHandle>>, batch: Vec<Submission>) {
	use std::collections::HashMap;

	// Op = the merged operation for a single key after BUNDLE coalescing.
	enum Op {
		Write(Val),
		Delete,
	}

	let mut merged: HashMap<Key, Op> = HashMap::new();
	let mut max_version = 0u64;
	let mut replies: Vec<oneshot::Sender<BatchOutcome>> = Vec::with_capacity(batch.len());

	for sub in batch {
		if sub.version > max_version {
			max_version = sub.version;
		}
		for (k, v) in sub.writes {
			merged.insert(k, Op::Write(v));
		}
		for k in sub.deletes {
			merged.insert(k, Op::Delete);
		}
		replies.push(sub.reply);
	}

	// Partition the merged map back into (writes, deletes) for Lance.
	let mut writes: Vec<(Key, Val)> = Vec::with_capacity(merged.len());
	let mut deletes: Vec<Key> = Vec::new();
	for (k, op) in merged {
		match op {
			Op::Write(v) => writes.push((k, v)),
			Op::Delete => deletes.push(k),
		}
	}

	// The legacy gate does not track a per-row commit seq (that lives on
	// the LSM memtable path); stamp every row with `max_version` so the
	// non-null `seq` column is populated consistently with `version`.
	let write_seqs = vec![max_version; writes.len()];
	let delete_seqs = vec![max_version; deletes.len()];
	let result =
		single_lance_commit(dataset, writes, write_seqs, deletes, delete_seqs, max_version).await;

	let outcome = match &result {
		Ok(()) => BatchOutcome::Ok,
		Err(e) => {
			warn!(target: TARGET, "batched lance commit failed: {e}");
			BatchOutcome::from_err(e)
		}
	};

	// Broadcast the outcome to every submitter. A dropped receiver is
	// expected if the caller's await was cancelled — ignore the send error.
	for reply in replies {
		let _ = reply.send(outcome.clone());
	}
}

/// Apply a batch's writes **and** deletes as a SINGLE Lance commit.
///
/// Writes become live rows (`tombstone = false`); deletes become
/// tombstone rows (`tombstone = true`). Both are streamed into one
/// `MergeInsertBuilder::execute_reader` keyed on `key`. `execute_batch`
/// coalesces every key to exactly one of write/delete, so the merge
/// source has unique keys and the upsert is well-defined - producing
/// exactly ONE dataset version per call, which concurrent submitters in
/// the same batch share.
///
/// Folding deletes in as tombstone rows (rather than a separate
/// `Dataset::delete`) is what keeps the Lance version history aligned
/// with SurrealDB commit boundaries. The old `merge_insert` +
/// `Dataset::delete` pair produced *two* versions for a write+delete
/// batch; the intermediate write-applied/delete-pending version, though
/// hidden from live readers by the write lock, leaked through
/// `Timeline::versions()` as a snapshot that was never an atomic commit.
async fn single_lance_commit(
	dataset: &Arc<RwLock<DatasetHandle>>,
	writes: Vec<(Key, Val)>,
	write_seqs: Vec<u64>,
	deletes: Vec<Key>,
	delete_seqs: Vec<u64>,
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
			super::Transaction::build_write_batch_lance(&writes, version, &write_seqs)
				.map_err(|e| Error::Datastore(format!("lance build batch: {e}")))?,
		);
	}
	if !deletes.is_empty() {
		batches.push(
			super::Transaction::build_tombstone_batch_lance(&deletes, version, &delete_seqs)
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

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn batch_outcome_round_trips() {
		assert!(matches!(BatchOutcome::Ok.into_result(), Ok(())));
		assert!(matches!(
			BatchOutcome::Conflict("x".into()).into_result(),
			Err(Error::TransactionConflict(_))
		));
		assert!(matches!(
			BatchOutcome::Datastore("y".into()).into_result(),
			Err(Error::Datastore(_))
		));
	}

	#[tokio::test]
	async fn batch_outcome_from_err_preserves_conflict() {
		let e = Error::TransactionConflict("oops".into());
		let outcome = BatchOutcome::from_err(&e);
		assert!(matches!(outcome, BatchOutcome::Conflict(_)));
		assert!(outcome.into_result().is_err_and(|e| e.is_retryable()));
	}

	#[tokio::test]
	async fn batch_outcome_from_err_collapses_to_datastore() {
		let e = Error::Internal("anything".into());
		let outcome = BatchOutcome::from_err(&e);
		assert!(matches!(outcome, BatchOutcome::Datastore(_)));
	}
}
