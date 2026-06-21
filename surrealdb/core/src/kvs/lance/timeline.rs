#![cfg(feature = "kv-lance")]
#![allow(dead_code)] // unwired, test-covered read surface (future kanban/replay consumer)

//! # Timeline — read-only time-series view into the Lance-backed SoA
//!
//! ## Role (the Rubicon ruling)
//!
//! lance-graph is the leading store / source of truth; SurrealDB is **one
//! VIEW over it** (the Rubicon kanban), **never a store**. This module is
//! the concrete surface for that ruling: a [`Timeline`] enumerates the
//! Lance dataset's native version history and hands out [`TimelineView`]s —
//! each an *immutable* snapshot pinned to one version. The view exposes
//! reads only; it owns no write path, so "SurrealDB never mutates the SoA"
//! is enforced by the type system, not by convention.
//!
//! ## What it builds on (all Lance 7.0.0 surface — the mandatory pin)
//!
//! - `Dataset::versions() -> Vec<lance::dataset::Version>` — the version
//!   timeline (same call the lance-graph `VersionedGraph` uses).
//! - `Dataset::checkout_version(u64)` — pin a read-only snapshot.
//! - `Dataset::version().version -> u64` — the latest version number.
//! - `Scanner::project(&["key","val","version"])` + `filter` — the same
//!   scan idiom as [`super::Transaction::scan_impl`].
//!
//! ## Wall-clock time (owned, not inferred)
//!
//! The `version` column written by [`super::schema::KvSchema`] carries the
//! MVCC version *at write time* (a `u64`). Wall-clock timestamps for the
//! kanban come from the Lance [`Version`] records' own ordering; we surface
//! the raw `version: u64` (monotone, gap-tolerant) as the canonical
//! timeline axis and expose the Lance-reported timestamp opportunistically.
//! Consumers that need a guaranteed wall-clock column should follow the
//! lance-graph `VersionedGraph` pattern of writing an owned
//! `timestamp_micros` column; the SurrealDB KV schema deliberately keeps
//! only `version` to stay byte-minimal.
//!
//! ## Additive shape
//!
//! New module; no existing item is modified. `Datastore::timeline()` is the
//! single new accessor (see `mod.rs`).

use std::sync::Arc;

use futures::TryStreamExt;
use tokio::sync::RwLock;

use super::DatasetHandle;
use super::schema::KvSchema;
use crate::kvs::err::{Error, Result};
use crate::kvs::{Key, Val};

/// One entry in the version history of the underlying Lance dataset.
///
/// `version` is the canonical, monotone (gap-tolerant) timeline axis. It is
/// the same `u64` that [`super::Transaction::get`]'s `version: Option<u64>`
/// parameter accepts, so any `VersionInfo` returned here can be fed straight
/// back into a point-in-time read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionInfo {
	/// Lance native dataset version number. Monotone non-decreasing across
	/// commits; gaps are permitted.
	pub version: u64,
	/// Lance-reported creation time of this version, in microseconds since
	/// the Unix epoch, when available. `None` if Lance does not surface a
	/// timestamp for the version.
	pub timestamp_us: Option<i64>,
}

/// A read-only time-series view over the Lance-backed SoA.
///
/// Constructed from the same `Arc<RwLock<DatasetHandle>>` the live
/// [`super::Datastore`] holds, so it observes committed history without a
/// second open. It exposes **no** write methods — the Rubicon "view, never
/// a store" invariant is structural.
#[derive(Clone)]
pub struct Timeline {
	dataset: Arc<RwLock<DatasetHandle>>,
}

impl Timeline {
	/// Build a timeline over the given dataset handle.
	pub(super) fn new(dataset: Arc<RwLock<DatasetHandle>>) -> Self {
		Self {
			dataset,
		}
	}

	/// The latest (current) version of the dataset.
	pub async fn latest_version(&self) -> u64 {
		self.dataset.read().await.inner.version().version
	}

	/// Enumerate the full version history, oldest → newest.
	///
	/// Mirrors `lance-graph::VersionedGraph::versions`: delegates to
	/// `Dataset::versions()` and projects each Lance `Version` onto the
	/// minimal [`VersionInfo`] the kanban needs.
	pub async fn versions(&self) -> Result<Vec<VersionInfo>> {
		let ds = self.dataset.read().await;
		let raw = ds
			.inner
			.versions()
			.await
			.map_err(|e| Error::Datastore(format!("lance timeline versions: {e}")))?;
		Ok(raw
			.into_iter()
			.map(|v| VersionInfo {
				version: v.version,
				// Lance `Version::timestamp` is a `chrono::DateTime<Utc>`;
				// fold it to epoch-micros for a dependency-light surface.
				// If a future Lance bump changes the shape, only this line
				// moves.
				timestamp_us: Some(v.timestamp.timestamp_micros()),
			})
			.collect())
	}

	/// Open an immutable view pinned to `version`.
	///
	/// Returns `Err` if the version does not exist in the dataset history
	/// (e.g. it was pruned by the background optimizer's
	/// `cleanup_old_versions`).
	pub async fn view_at(&self, version: u64) -> Result<TimelineView> {
		let ds = self.dataset.read().await;
		let snapshot = ds
			.inner
			.checkout_version(version)
			.await
			.map_err(|e| Error::Datastore(format!("lance timeline checkout {version}: {e}")))?;
		Ok(TimelineView {
			version,
			snapshot,
		})
	}

	/// Open an immutable view pinned to the latest version.
	pub async fn view_latest(&self) -> Result<TimelineView> {
		let version = self.latest_version().await;
		self.view_at(version).await
	}
}

/// An immutable snapshot of the SoA at one version.
///
/// Holds a checked-out Lance dataset (`checkout_version` yields an owned,
/// read-pinned `Dataset`). All methods are read-only; there is deliberately
/// no `set`/`del`/`commit` here.
#[allow(dead_code)] // unwired read surface for a future kanban/replay consumer
pub struct TimelineView {
	version: u64,
	snapshot: lance::Dataset,
}

#[allow(dead_code)]
impl TimelineView {
	/// The version this view is pinned to.
	pub fn version(&self) -> u64 {
		self.version
	}

	/// Point read of a single key as it existed at this version.
	///
	/// Skips tombstoned rows (a delete recorded at-or-before this version
	/// reads back as absent), matching [`super::Transaction::get`]'s
	/// historical-read semantics.
	pub async fn get(&self, key: &Key) -> Result<Option<Val>> {
		let filter = KvSchema::build_get_predicate(key);
		let mut scanner = self.snapshot.scan();
		scanner
			.filter(&filter)
			.map_err(|e| Error::Datastore(format!("lance timeline get filter: {e}")))?
			.project(&["val", "tombstone"])
			.map_err(|e| Error::Datastore(format!("lance timeline get project: {e}")))?
			.limit(Some(1), None)
			.map_err(|e| Error::Datastore(format!("lance timeline get limit: {e}")))?;

		let mut stream = scanner
			.try_into_stream()
			.await
			.map_err(|e| Error::Datastore(format!("lance timeline get stream: {e}")))?;

		while let Some(batch) = stream
			.try_next()
			.await
			.map_err(|e| Error::Datastore(format!("lance timeline get next: {e}")))?
		{
			if batch.num_rows() == 0 {
				continue;
			}
			let tombstone_col = batch
				.column_by_name("tombstone")
				.ok_or_else(|| Error::Datastore("lance timeline get: missing tombstone".into()))?
				.as_any()
				.downcast_ref::<arrow_array::BooleanArray>()
				.ok_or_else(|| {
					Error::Datastore("lance timeline get: tombstone type mismatch".into())
				})?;
			if tombstone_col.value(0) {
				return Ok(None);
			}
			let val_col = batch
				.column_by_name("val")
				.ok_or_else(|| Error::Datastore("lance timeline get: missing val".into()))?
				.as_any()
				.downcast_ref::<arrow_array::BinaryArray>()
				.ok_or_else(|| Error::Datastore("lance timeline get: val type mismatch".into()))?;
			return Ok(Some(val_col.value(0).to_vec()));
		}
		Ok(None)
	}

	/// Scan all live `(key, val)` pairs visible at this version, ascending
	/// by key. Tombstoned keys are omitted.
	///
	/// This is the "transparent view into the SoA" surface: a downstream
	/// kanban / replay consumer walks `timeline.versions()` and calls
	/// `view_at(v).scan()` to materialise the SoA as it stood at each
	/// Rubicon commit — without any write path back into Lance.
	pub async fn scan(&self) -> Result<Vec<(Key, Val)>> {
		let mut scanner = self.snapshot.scan();
		scanner
			.project(&["key", "val", "tombstone"])
			.map_err(|e| Error::Datastore(format!("lance timeline scan project: {e}")))?;

		let mut stream = scanner
			.try_into_stream()
			.await
			.map_err(|e| Error::Datastore(format!("lance timeline scan stream: {e}")))?;

		let mut out: Vec<(Key, Val)> = Vec::new();
		while let Some(batch) = stream
			.try_next()
			.await
			.map_err(|e| Error::Datastore(format!("lance timeline scan next: {e}")))?
		{
			let key_col = batch
				.column_by_name("key")
				.ok_or_else(|| Error::Datastore("lance timeline scan: missing key".into()))?
				.as_any()
				.downcast_ref::<arrow_array::BinaryArray>()
				.ok_or_else(|| Error::Datastore("lance timeline scan: key type mismatch".into()))?;
			let val_col = batch
				.column_by_name("val")
				.ok_or_else(|| Error::Datastore("lance timeline scan: missing val".into()))?
				.as_any()
				.downcast_ref::<arrow_array::BinaryArray>()
				.ok_or_else(|| Error::Datastore("lance timeline scan: val type mismatch".into()))?;
			let tombstone_col = batch
				.column_by_name("tombstone")
				.ok_or_else(|| Error::Datastore("lance timeline scan: missing tombstone".into()))?
				.as_any()
				.downcast_ref::<arrow_array::BooleanArray>()
				.ok_or_else(|| {
					Error::Datastore("lance timeline scan: tombstone type mismatch".into())
				})?;

			for i in 0..batch.num_rows() {
				if tombstone_col.value(i) {
					continue;
				}
				out.push((key_col.value(i).to_vec(), val_col.value(i).to_vec()));
			}
		}
		out.sort_by(|a, b| a.0.cmp(&b.0));
		Ok(out)
	}
}
