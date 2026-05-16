#![cfg(feature = "kv-lance")]

//! Arrow schema for the SurrealDB KV pair layout on top of Lance.
//!
//! The schema is intentionally **minimal and opaque-binary**: SurrealDB
//! produces structured binary keys (`namespace|database|table|record_id|...`)
//! that we store as-is. Lance still gives us O(log n) point lookup via a
//! BTREE scalar index on the `key` column.
//!
//! ## Schema
//!
//! ```text
//!  key:        Binary       — opaque SurrealDB binary key. NOT NULL.
//!                            BTREE scalar index.
//!  val:        Binary       — opaque SurrealDB binary value. NOT NULL.
//!  version:    UInt64       — Lance dataset version at write time;
//!                            stored for MVCC point-lookup convenience
//!                            (avoids a checkout for the latest-row case).
//!  tombstone:  Boolean      — true means this row marks a deletion at
//!                            this version. Lance's native deletion
//!                            vectors handle this too, but we keep an
//!                            explicit column for visibility in scans.
//! ```
//!
//! ## Future extension: typed columns
//!
//! When latency permits, the `key` field could be parsed into typed
//! sub-columns (e.g. `namespace_id: UInt32`, `table_name: String`,
//! `record_id: Binary`) to enable Lance column-pruning on typed
//! predicates. This is a follow-up optimisation, not part of the POC.

use std::sync::Arc;

use arrow_array::{
	BinaryArray, BooleanArray, RecordBatch, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};

use crate::kvs::{Key, Val};

/// Logical handle for the SurrealDB-on-Lance KV schema.
pub struct KvSchema;

// These methods build the Arrow schema and record batches used by the commit
// path (Sprint II+) and the tombstone-row strategy.  The call sites live in
// Transaction::commit which is not yet wired; suppress dead_code until then.
#[allow(dead_code)]
impl KvSchema {
	/// Build the Arrow schema for the SurrealDB KV layer on Lance.
	pub fn arrow_schema() -> Schema {
		Schema::new(vec![
			Field::new("key", DataType::Binary, false),
			Field::new("val", DataType::Binary, false),
			Field::new("version", DataType::UInt64, false),
			Field::new("tombstone", DataType::Boolean, false),
		])
	}

	/// Reference-counted version of [`Self::arrow_schema`].
	pub fn arrow_schema_ref() -> Arc<Schema> {
		Arc::new(Self::arrow_schema())
	}

	/// Build a `RecordBatch` from a list of `(key, val)` pairs at a
	/// given version. All rows have `tombstone = false`.
	///
	/// Used by `Transaction::commit` to materialise pending writes
	/// into a single Lance append-batch.
	pub fn build_write_batch(
		writes: &[(Key, Val)],
		version: u64,
	) -> Result<RecordBatch, arrow_schema::ArrowError> {
		let schema = Self::arrow_schema_ref();

		let key_array: BinaryArray = writes
			.iter()
			.map(|(k, _)| Some(k.as_slice()))
			.collect();
		let val_array: BinaryArray = writes
			.iter()
			.map(|(_, v)| Some(v.as_slice()))
			.collect();
		let version_array: UInt64Array =
			std::iter::repeat_n(version, writes.len()).collect();
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

	/// Build a `RecordBatch` of tombstone rows for the given keys.
	///
	/// Lance has native deletion vectors via `Dataset::delete(predicate)`,
	/// but we sometimes prefer to write explicit tombstone rows so that
	/// `scan` can see "deleted at version v" as data, not as predicate.
	/// The commit logic chooses one or the other strategy based on the
	/// `LANCE_DELETE_VIA_TOMBSTONE_ROW` config knob.
	pub fn build_tombstone_batch(
		deletes: &[Key],
		version: u64,
	) -> Result<RecordBatch, arrow_schema::ArrowError> {
		let schema = Self::arrow_schema_ref();
		let empty_val: &[u8] = &[];

		let key_array: BinaryArray =
			deletes.iter().map(|k| Some(k.as_slice())).collect();
		let val_array: BinaryArray =
			std::iter::repeat_n(Some(empty_val), deletes.len()).collect();
		let version_array: UInt64Array =
			std::iter::repeat_n(version, deletes.len()).collect();
		let tombstone_array = BooleanArray::from(vec![true; deletes.len()]);

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

	/// Build a DataFusion predicate string for `Dataset::delete()` that
	/// matches the given keys via `key IN (X'aabb', X'ccdd', ...)`.
	pub fn build_delete_predicate(deletes: &[Key]) -> String {
		if deletes.is_empty() {
			return "false".to_string();
		}
		let hex_keys: Vec<String> =
			deletes.iter().map(|k| format!("X'{}'", hex::encode(k))).collect();
		format!("key IN ({})", hex_keys.join(", "))
	}

	/// Build a DataFusion predicate for `key = X'aabb' AND tombstone = false`.
	/// Used by `Transaction::get`.
	pub fn build_get_predicate(key: &Key) -> String {
		format!("key = X'{}' AND tombstone = false", hex::encode(key))
	}

	/// Build a DataFusion predicate for the half-open key range
	/// `[start, end)` excluding tombstones. Used by `scan` / `keys`.
	pub fn build_range_predicate(start: &Key, end: &Key) -> String {
		format!(
			"key >= X'{}' AND key < X'{}' AND tombstone = false",
			hex::encode(start),
			hex::encode(end),
		)
	}
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn schema_has_expected_fields() {
		let schema = KvSchema::arrow_schema();
		assert_eq!(schema.fields().len(), 4);
		assert_eq!(schema.field(0).name(), "key");
		assert_eq!(schema.field(1).name(), "val");
		assert_eq!(schema.field(2).name(), "version");
		assert_eq!(schema.field(3).name(), "tombstone");
	}

	#[test]
	fn write_batch_roundtrip() {
		let writes = vec![
			(b"key1".to_vec(), b"val1".to_vec()),
			(b"key2".to_vec(), b"val2".to_vec()),
		];
		let batch = KvSchema::build_write_batch(&writes, 42).unwrap();
		assert_eq!(batch.num_rows(), 2);
		assert_eq!(batch.num_columns(), 4);
	}

	#[test]
	fn tombstone_batch_marks_deletion() {
		let deletes = vec![b"key1".to_vec()];
		let batch = KvSchema::build_tombstone_batch(&deletes, 42).unwrap();
		let tombstone_col = batch
			.column_by_name("tombstone")
			.unwrap()
			.as_any()
			.downcast_ref::<BooleanArray>()
			.unwrap();
		assert!(tombstone_col.value(0));
	}

	#[test]
	fn get_predicate_hex_encodes_key() {
		let key = vec![0xaa, 0xbb, 0xcc];
		let pred = KvSchema::build_get_predicate(&key);
		assert_eq!(pred, "key = X'aabbcc' AND tombstone = false");
	}

	#[test]
	fn range_predicate_is_half_open() {
		let start = vec![0x01];
		let end = vec![0x05];
		let pred = KvSchema::build_range_predicate(&start, &end);
		assert!(pred.contains("key >= X'01'"));
		assert!(pred.contains("key < X'05'"));
	}
}
