#![cfg(feature = "kv-lance")]

//! Pending-writes buffer for in-flight Lance transactions.
//!
//! Lance has no per-row transaction buffer like SurrealKV's `Tree.set`.
//! Instead, we accumulate writes/deletes in this buffer and flush them
//! atomically on [`Transaction::commit`].

use std::collections::HashMap;

use crate::kvs::{Key, Val};

/// A single pending entry: either a write (Set) or a deletion.
#[derive(Debug, Clone)]
pub enum PendingEntry {
	Set(Val),
	Delete,
}

/// In-memory buffer of writes/deletes for a single in-flight transaction.
///
/// Keyed by `Vec<u8>` (the SurrealDB binary key). We use `HashMap` for
/// O(1) lookup during `get(key)` read-your-writes; ordering is recovered
/// at commit time when we build the Arrow batch.
///
/// `clone()` is used by [`Transaction::new_save_point`] to snapshot the
/// buffer. `HashMap::clone` is `O(n)` in the buffer size but typically
/// fast in practice (transactions rarely hold tens of thousands of
/// pending writes).
#[derive(Debug, Clone, Default)]
pub struct PendingBuffer {
	entries: HashMap<Key, PendingEntry>,
}

impl PendingBuffer {
	pub fn new() -> Self {
		Self::default()
	}

	/// Insert/overwrite a Set entry for `key`.
	pub fn set(&mut self, key: Key, val: Val) {
		self.entries.insert(key, PendingEntry::Set(val));
	}

	/// Mark `key` as a Delete tombstone.
	pub fn delete(&mut self, key: Key) {
		self.entries.insert(key, PendingEntry::Delete);
	}

	/// Look up an entry by key (returns `None` if not buffered).
	pub fn get(&self, key: &Key) -> Option<&PendingEntry> {
		self.entries.get(key)
	}

	/// Drop all pending entries (used by `cancel()` and `clear` paths).
	pub fn clear(&mut self) {
		self.entries.clear();
	}

	/// Number of pending entries.
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// Partition the buffer into writes (`Set`) and deletes (`Delete`).
	///
	/// Returns owned `Vec`s ready to feed into Arrow `RecordBatch` builders.
	/// The current implementation iterates the HashMap once; for very
	/// large buffers (>100k entries) a streaming variant could be added.
	pub fn partition(&self) -> (Vec<(Key, Val)>, Vec<Key>) {
		let mut writes = Vec::with_capacity(self.entries.len());
		let mut deletes = Vec::new();
		for (k, v) in &self.entries {
			match v {
				PendingEntry::Set(val) => writes.push((k.clone(), val.clone())),
				PendingEntry::Delete => deletes.push(k.clone()),
			}
		}
		(writes, deletes)
	}

	/// Iterate all entries (used by scan-merge logic).
	pub fn iter(&self) -> impl Iterator<Item = (&Key, &PendingEntry)> {
		self.entries.iter()
	}
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn set_then_get_returns_set() {
		let mut buf = PendingBuffer::new();
		buf.set(b"key".to_vec(), b"val".to_vec());
		match buf.get(&b"key".to_vec()) {
			Some(PendingEntry::Set(v)) => assert_eq!(v, b"val"),
			other => panic!("expected Set, got {:?}", other),
		}
	}

	#[test]
	fn delete_then_get_returns_delete() {
		let mut buf = PendingBuffer::new();
		buf.delete(b"key".to_vec());
		assert!(matches!(buf.get(&b"key".to_vec()), Some(PendingEntry::Delete)));
	}

	#[test]
	fn set_then_delete_overrides_set() {
		let mut buf = PendingBuffer::new();
		buf.set(b"key".to_vec(), b"val".to_vec());
		buf.delete(b"key".to_vec());
		assert!(matches!(buf.get(&b"key".to_vec()), Some(PendingEntry::Delete)));
	}

	#[test]
	fn partition_separates_writes_and_deletes() {
		let mut buf = PendingBuffer::new();
		buf.set(b"a".to_vec(), b"1".to_vec());
		buf.set(b"b".to_vec(), b"2".to_vec());
		buf.delete(b"c".to_vec());

		let (writes, deletes) = buf.partition();
		assert_eq!(writes.len(), 2);
		assert_eq!(deletes.len(), 1);
		assert_eq!(deletes[0], b"c");
	}

	#[test]
	fn clone_for_savepoint_is_deep() {
		let mut buf = PendingBuffer::new();
		buf.set(b"a".to_vec(), b"1".to_vec());
		let snapshot = buf.clone();
		buf.set(b"a".to_vec(), b"2".to_vec());

		match snapshot.get(&b"a".to_vec()) {
			Some(PendingEntry::Set(v)) => assert_eq!(v, b"1"),
			_ => panic!("snapshot was not deep-cloned"),
		}
	}
}
