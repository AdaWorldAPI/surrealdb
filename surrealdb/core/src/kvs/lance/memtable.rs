#![cfg(feature = "kv-lance")]

//! In-memory write buffer for the kv-lance backend.
//!
//! Pairs with [`super::wal::Wal`] to form the LSM-style hot path:
//!
//! ```text
//!   writer → WAL.append (fsync) → Memtable.insert → reply Ok
//!   reader → Memtable.get → fall through to Lance scan if miss
//!   flusher → Memtable.drain_up_to(gen) → Lance MergeInsertBuilder
//!           → WAL.truncate_to(gen + 1)
//! ```
//!
//! ## Concurrency
//!
//! Backed by [`dashmap::DashMap`] so concurrent commits don't
//! serialize on a single lock — each shard of the map can be
//! mutated independently. Generation numbers are minted from an
//! [`AtomicU64`] so the per-commit ordering is monotonic across all
//! writers without coordination.
//!
//! ## Semantics
//!
//! - The key is the SurrealDB binary key.
//! - The value is a [`MemtableEntry`] = (op, generation).
//! - `insert` of the same key OVERWRITES the previous entry. The
//!   higher generation wins. Last-write-wins is consistent with
//!   `MergeMode::Bundle` from `lance-graph`'s CollapseGate; it also
//!   matches upstream LSM-tree backends where the latest write to a
//!   key shadows the earlier values.
//! - `Delete` is stored as an explicit `Op::Delete` tombstone rather
//!   than a remove from the map. Readers see "this key was deleted
//!   at gen N" and treat it as `None`.
//! - On flush, both `Set` rows and `Delete` rows are drained — the
//!   `Set` rows become Lance merge-insert source, the `Delete` rows
//!   become a `Dataset::delete(predicate)` call.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use crate::kvs::{Key, Val};

/// What happened to a key inside a memtable entry.
#[derive(Debug, Clone)]
pub(super) enum Op {
	Set(Val),
	Delete,
}

/// One key's worth of pending state in the memtable.
#[derive(Debug, Clone)]
pub(super) struct MemtableEntry {
	pub op: Op,
	pub generation: u64,
}

/// Concurrent in-memory write buffer for the kv-lance backend.
pub(super) struct Memtable {
	entries: DashMap<Key, MemtableEntry>,
	/// Monotonic generation counter. Each commit calls
	/// `next_generation()` exactly once before appending to the WAL
	/// and inserting into the memtable, so a record's generation
	/// matches its commit order.
	generation: AtomicU64,
}

impl Memtable {
	pub(super) fn new() -> Arc<Self> {
		Arc::new(Self {
			entries: DashMap::new(),
			generation: AtomicU64::new(0),
		})
	}

	/// Allocate a new monotonic generation number. Called once per
	/// commit before WAL append + memtable insert.
	pub(super) fn next_generation(&self) -> u64 {
		// `Relaxed` is sufficient: the only ordering requirement is
		// monotonicity per-call, not happens-before with respect to
		// any other memory location.
		self.generation.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
	}

	/// Insert / overwrite an entry. The newer-generation entry wins
	/// the contended-key race; callers must always pass a generation
	/// they just minted from [`Self::next_generation`].
	pub(super) fn insert(&self, key: Key, op: Op, generation: u64) {
		self.entries
			.entry(key)
			.and_modify(|existing| {
				if generation > existing.generation {
					existing.op = op.clone();
					existing.generation = generation;
				}
			})
			.or_insert(MemtableEntry { op, generation });
	}

	/// Look up an entry by key. Returns `None` if no in-memory write
	/// exists; the caller should then check the Lance dataset.
	pub(super) fn get(&self, key: &[u8]) -> Option<MemtableEntry> {
		self.entries.get(key).map(|e| e.value().clone())
	}

	/// Iterate every entry whose key falls within `[range.start,
	/// range.end)`. Order is unspecified — the caller (typically
	/// `Transaction::scan_impl`) merges with the Lance scan and
	/// re-sorts.
	pub(super) fn scan_range(
		&self,
		range: &Range<Key>,
	) -> Vec<(Key, MemtableEntry)> {
		let mut out = Vec::new();
		for kv in self.entries.iter() {
			let k = kv.key();
			if k.as_slice() >= range.start.as_slice()
				&& k.as_slice() < range.end.as_slice()
			{
				out.push((k.clone(), kv.value().clone()));
			}
		}
		out
	}

	/// Snapshot all entries with `generation <= up_to_generation`,
	/// returning (key, op) pairs. The matching entries remain in the
	/// memtable; the flusher is expected to call
	/// [`Self::drop_committed`] only AFTER the Lance commit succeeds
	/// so that readers still see the entries while the flush is in
	/// flight.
	///
	/// Returning a cloned snapshot avoids holding any dashmap shard
	/// locks across the Lance write.
	pub(super) fn snapshot_up_to(
		&self,
		up_to_generation: u64,
	) -> Vec<(Key, MemtableEntry)> {
		let mut out = Vec::new();
		for kv in self.entries.iter() {
			let e = kv.value();
			if e.generation <= up_to_generation {
				out.push((kv.key().clone(), e.clone()));
			}
		}
		out
	}

	/// After a successful flush, drop entries that have been
	/// persisted. The check is per-entry generation: only drop if the
	/// memtable's CURRENT entry for the key is still ≤ `up_to`,
	/// because a newer write may have raced the flush and updated
	/// the entry's generation while we were committing to Lance.
	pub(super) fn drop_committed(&self, flushed: &[(Key, u64)]) {
		for (k, generation) in flushed {
			// `remove_if` keeps the entry when the closure returns
			// false; we want to drop only if the entry hasn't been
			// touched by a newer commit.
			self.entries.remove_if(k, |_k, v| v.generation <= *generation);
		}
	}

	/// Approximate number of entries (cheap; lock-free).
	pub(super) fn len(&self) -> usize {
		self.entries.len()
	}

	pub(super) fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// Highest generation observed by the counter, NOT necessarily
	/// the highest generation present in the map. Used by the
	/// flusher to decide its `up_to` bound.
	pub(super) fn current_generation(&self) -> u64 {
		self.generation.load(Ordering::Relaxed)
	}
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn generations_are_monotonic() {
		let m = Memtable::new();
		let g1 = m.next_generation();
		let g2 = m.next_generation();
		let g3 = m.next_generation();
		assert!(g2 > g1);
		assert!(g3 > g2);
	}

	#[test]
	fn insert_then_get_returns_set() {
		let m = Memtable::new();
		let g = m.next_generation();
		m.insert(b"k".to_vec(), Op::Set(b"v".to_vec()), g);
		let e = m.get(b"k").expect("present");
		match e.op {
			Op::Set(v) => assert_eq!(v, b"v"),
			Op::Delete => panic!("expected Set"),
		}
		assert_eq!(e.generation, g);
	}

	#[test]
	fn newer_generation_wins() {
		let m = Memtable::new();
		let g1 = m.next_generation();
		let g2 = m.next_generation();
		m.insert(b"k".to_vec(), Op::Set(b"old".to_vec()), g1);
		m.insert(b"k".to_vec(), Op::Set(b"new".to_vec()), g2);
		match m.get(b"k").expect("present").op {
			Op::Set(v) => assert_eq!(v, b"new"),
			Op::Delete => panic!("expected Set"),
		}
	}

	#[test]
	fn older_generation_does_not_overwrite() {
		// Defends against out-of-order inserts (e.g. WAL replay).
		let m = Memtable::new();
		let g1 = m.next_generation();
		let g2 = m.next_generation();
		m.insert(b"k".to_vec(), Op::Set(b"new".to_vec()), g2);
		m.insert(b"k".to_vec(), Op::Set(b"older".to_vec()), g1);
		match m.get(b"k").expect("present").op {
			Op::Set(v) => assert_eq!(v, b"new"),
			Op::Delete => panic!("expected Set"),
		}
	}

	#[test]
	fn delete_is_tombstone() {
		let m = Memtable::new();
		let g1 = m.next_generation();
		let g2 = m.next_generation();
		m.insert(b"k".to_vec(), Op::Set(b"v".to_vec()), g1);
		m.insert(b"k".to_vec(), Op::Delete, g2);
		assert!(matches!(m.get(b"k").expect("present").op, Op::Delete));
	}

	#[test]
	fn scan_range_returns_only_matching_keys() {
		let m = Memtable::new();
		let g = m.next_generation();
		m.insert(b"aaa".to_vec(), Op::Set(b"v".to_vec()), g);
		m.insert(b"bbb".to_vec(), Op::Set(b"v".to_vec()), g);
		m.insert(b"ccc".to_vec(), Op::Set(b"v".to_vec()), g);
		let range = b"aaa".to_vec()..b"ccc".to_vec();
		let mut keys: Vec<_> = m
			.scan_range(&range)
			.into_iter()
			.map(|(k, _)| k)
			.collect();
		keys.sort();
		assert_eq!(keys, vec![b"aaa".to_vec(), b"bbb".to_vec()]);
	}

	#[test]
	fn snapshot_then_drop_committed_removes_only_flushed() {
		let m = Memtable::new();
		let g1 = m.next_generation();
		let g2 = m.next_generation();
		let g3 = m.next_generation();
		m.insert(b"a".to_vec(), Op::Set(b"1".to_vec()), g1);
		m.insert(b"b".to_vec(), Op::Set(b"2".to_vec()), g2);
		m.insert(b"c".to_vec(), Op::Set(b"3".to_vec()), g3);

		// Flusher snapshots up to g2 (drops a and b only).
		let snap = m.snapshot_up_to(g2);
		assert_eq!(snap.len(), 2);

		// Simulate a successful Lance flush of the snapshotted rows.
		let flushed: Vec<_> =
			snap.into_iter().map(|(k, e)| (k, e.generation)).collect();
		m.drop_committed(&flushed);

		assert_eq!(m.len(), 1, "only g=3 should remain");
		assert!(m.get(b"c").is_some());
		assert!(m.get(b"a").is_none());
		assert!(m.get(b"b").is_none());
	}

	#[test]
	fn drop_committed_keeps_race_winners() {
		// Scenario: flusher snapshots key 'a' at g=5. While the Lance
		// commit is in flight, a new write to 'a' lands at g=7. After
		// the commit completes, `drop_committed` is called with
		// (key='a', flushed_gen=5). The memtable's current entry is
		// at g=7 (newer), so it must be RETAINED — otherwise we'd
		// lose the racy write.
		let m = Memtable::new();
		let g_flushed = m.next_generation(); // 1
		let _g_intermediate = m.next_generation(); // 2
		let g_raced = m.next_generation(); // 3

		// Race winner is already in the memtable when drop_committed
		// runs:
		m.insert(b"a".to_vec(), Op::Set(b"racy".to_vec()), g_raced);

		// Flusher's snapshot had captured g_flushed; it now asks
		// memtable to drop entries up to g_flushed for key 'a'.
		m.drop_committed(&[(b"a".to_vec(), g_flushed)]);

		// The racy g_raced write must still be there.
		let still = m.get(b"a").expect("racy winner survived");
		assert_eq!(still.generation, g_raced);
	}
}
