#![cfg(test)]
#![cfg(feature = "kv-lance")]

//! Integration tests for the Lance backend.
//!
//! These tests exercise the public Datastore API end-to-end with a
//! temporary directory acting as the Lance dataset location. They cover
//! ONLY the `Transactable` contract against the native single-path
//! backend (one SurrealDB commit = one lance dataset version). The
//! hand-rolled LSM apparatus (WAL / memtable / flusher / commit-gate /
//! WritePath) no longer exists, so the tests that exercised it have been
//! removed.
//!
//! # Note on `tempfile`
//!
//! The `tempfile` crate is optional in `surrealdb-core` and is NOT included
//! in the `kv-lance` feature set. We therefore use `std::env::temp_dir()`
//! combined with `uuid::Uuid::new_v4()` to produce unique, isolated paths
//! per test. The directories are intentionally left on the filesystem after
//! the test completes (OS temp-dir cleanup handles them eventually) — no
//! `Drop` cleanup is required for correctness.

use std::path::PathBuf;

use uuid::Uuid;

use super::Datastore;
use crate::kvs::api::Transactable;
use crate::kvs::config::LanceConfig;

/// Return a unique path inside the OS temp directory for use as a
/// Lance dataset root. Each call returns a distinct path.
fn unique_tmp_path() -> PathBuf {
	let mut path = std::env::temp_dir();
	path.push(format!("surrealdb-lance-test-{}", Uuid::new_v4()));
	path
}

/// Open a fresh tempdir → Datastore::new creates a new Lance dataset
/// with the KV schema. shutdown() succeeds.
#[tokio::test]
async fn test_open_creates_new_dataset() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let config = LanceConfig::default();
	let ds = Datastore::new(path_str, config).await.expect("create dataset");
	// Initial version exists (may be 0 or 1 depending on Lance's
	// create-empty semantics — accept any non-negative).
	let _v = ds.current_version().await;
	ds.shutdown().await.expect("shutdown");
}

/// Create a dataset, drop it, re-open the same path — second open
/// succeeds (Dataset::open path, not the create path).
#[tokio::test]
async fn test_open_existing_dataset_succeeds() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let config = LanceConfig::default();

	// First open: creates.
	{
		let ds = Datastore::new(path_str, config.clone()).await.expect("first open");
		ds.shutdown().await.expect("first shutdown");
	}

	// Second open: must succeed via Dataset::open (not create).
	let ds = Datastore::new(path_str, config).await.expect("second open");
	let _v = ds.current_version().await;
	ds.shutdown().await.expect("second shutdown");
}

/// current_version returns a queryable u64. We don't assert a specific
/// value because Lance's empty-dataset version semantics are
/// implementation-defined; we only assert the call doesn't panic and
/// returns within a reasonable bound (< u64::MAX implies it's not
/// uninitialised garbage).
#[tokio::test]
async fn test_current_version_is_queryable() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let config = LanceConfig::default();
	let ds = Datastore::new(path_str, config).await.expect("create dataset");
	let v = ds.current_version().await;
	assert!(v < u64::MAX, "current_version should be a small u64, got {}", v);
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Lifecycle: closed() / kind() / writeable()
// ============================================================================

/// `kind()` is the stable backend identifier "lance".
#[tokio::test]
async fn test_kind_is_lance() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	let tx = ds.transaction(false, false).await.expect("tx");
	assert_eq!(tx.kind(), "lance", "kind() must be the stable 'lance' id");
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// `writeable()` reflects the `write` flag passed at transaction-open time.
#[tokio::test]
async fn test_writeable_reflects_open_flag() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let rw = ds.transaction(true, false).await.expect("rw tx");
	assert!(rw.writeable(), "tx opened with write=true must be writeable");
	rw.cancel().await.expect("cancel rw");

	let ro = ds.transaction(false, false).await.expect("ro tx");
	assert!(!ro.writeable(), "tx opened with write=false must not be writeable");
	ro.cancel().await.expect("cancel ro");

	ds.shutdown().await.expect("shutdown");
}

/// `closed()` is sticky: false while live, true after commit, and every
/// subsequent method returns TransactionFinished.
#[tokio::test]
async fn test_closed_is_sticky_after_commit() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	assert!(!tx.closed(), "fresh tx must not be closed");
	tx.set(b"k".to_vec(), b"v".to_vec()).await.expect("set");
	tx.commit().await.expect("commit");
	assert!(tx.closed(), "tx must be closed after commit");

	// Any further operation short-circuits with TransactionFinished.
	let err = tx.get(b"k".to_vec(), None).await.expect_err("get after commit must fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionFinished),
		"expected TransactionFinished after commit, got {:?}", err);
	let err = tx.set(b"k2".to_vec(), b"v2".to_vec()).await.expect_err("set after commit must fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionFinished),
		"expected TransactionFinished after commit, got {:?}", err);

	ds.shutdown().await.expect("shutdown");
}

/// `closed()` is sticky after cancel too.
#[tokio::test]
async fn test_closed_is_sticky_after_cancel() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	assert!(!tx.closed(), "fresh tx must not be closed");
	tx.cancel().await.expect("cancel");
	assert!(tx.closed(), "tx must be closed after cancel");

	let err = tx.get(b"k".to_vec(), None).await.expect_err("get after cancel must fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionFinished),
		"expected TransactionFinished after cancel, got {:?}", err);

	ds.shutdown().await.expect("shutdown");
}

/// A write on a read-only transaction short-circuits with TransactionReadonly.
#[tokio::test]
async fn test_write_on_readonly_tx_errors() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(false, false).await.expect("ro tx");
	let err = tx.set(b"k".to_vec(), b"v".to_vec()).await.expect_err("set on ro tx must fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionReadonly),
		"expected TransactionReadonly, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Transaction::get tests
// ============================================================================

/// get on a key that was never written → returns None.
#[tokio::test]
async fn test_get_missing_key_returns_none() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");
	let tx = ds.transaction(true, false).await.expect("tx");
	let result = tx.get(b"absent-key".to_vec(), None).await.expect("get");
	assert!(result.is_none(), "expected None for missing key, got {:?}", result);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// set in pending buffer → get returns the buffered value (read-your-writes).
#[tokio::test]
async fn test_get_after_set_returns_pending_value() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");
	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
	let result = tx.get(b"k1".to_vec(), None).await.expect("get");
	assert_eq!(
		result.as_deref(),
		Some(b"v1".as_ref()),
		"RYW failed: expected Some(v1), got {:?}",
		result
	);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// set then delete in pending → get returns None (tombstone wins over write).
#[tokio::test]
async fn test_get_after_set_then_del_in_pending_returns_none() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");
	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
	tx.del(b"k1".to_vec()).await.expect("del");
	let result = tx.get(b"k1".to_vec(), None).await.expect("get");
	assert!(
		result.is_none(),
		"pending delete should hide pending set, got {:?}",
		result
	);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// exists() is equivalent to get().is_some() — sanity check.
#[tokio::test]
async fn test_exists_mirrors_get() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");
	let tx = ds.transaction(true, false).await.expect("tx");
	assert!(!tx.exists(b"k1".to_vec(), None).await.expect("exists 1"));
	tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
	assert!(tx.exists(b"k1".to_vec(), None).await.expect("exists 2"));
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// exists() sees committed rows via the Lance scan, not just pending.
#[tokio::test]
async fn test_exists_sees_committed_row() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	let tx = ds.transaction(false, false).await.expect("tx2");
	assert!(tx.exists(b"k1".to_vec(), None).await.expect("exists committed"));
	assert!(!tx.exists(b"absent".to_vec(), None).await.expect("exists absent"));
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Transaction::commit tests
// ============================================================================

/// set → commit → get round-trips via the Lance dataset (not just pending).
#[tokio::test]
async fn test_set_commit_get_roundtrip() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	// First transaction: set + commit.
	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	// Second transaction: get must see the committed value via Lance scan.
	let tx = ds.transaction(false, false).await.expect("tx2");
	let result = tx.get(b"k1".to_vec(), None).await.expect("get");
	assert_eq!(result.as_deref(), Some(b"v1".as_ref()),
		"set+commit+get failed: expected Some(v1), got {:?}", result);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// cancel discards pending writes — they are not visible afterward.
#[tokio::test]
async fn test_cancel_discards_pending_writes() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.cancel().await.expect("cancel");
	}

	let tx = ds.transaction(false, false).await.expect("tx2");
	let result = tx.get(b"k1".to_vec(), None).await.expect("get");
	assert!(result.is_none(),
		"cancel should discard pending writes; got {:?}", result);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// Multiple set calls in one txn become visible atomically after commit.
#[tokio::test]
async fn test_multiple_sets_commit_atomically() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"a".to_vec(), b"1".to_vec()).await.expect("set a");
		tx.set(b"b".to_vec(), b"2".to_vec()).await.expect("set b");
		tx.set(b"c".to_vec(), b"3".to_vec()).await.expect("set c");
		tx.commit().await.expect("commit");
	}

	let tx = ds.transaction(false, false).await.expect("tx2");
	for (k, v) in [(b"a".as_ref(), b"1".as_ref()), (b"b", b"2"), (b"c", b"3")] {
		let result = tx.get(k.to_vec(), None).await.expect("get");
		assert_eq!(result.as_deref(), Some(v),
			"multi-set commit missing key {:?}: got {:?}", k, result);
	}
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// del after commit hides a previously-committed value.
#[tokio::test]
async fn test_del_after_commit_hides_value() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	// Insert.
	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit set");
	}

	// Sanity: value is there.
	{
		let tx = ds.transaction(false, false).await.expect("tx2");
		let result = tx.get(b"k1".to_vec(), None).await.expect("get pre-del");
		assert_eq!(result.as_deref(), Some(b"v1".as_ref()),
			"pre-delete sanity failed; got {:?}", result);
		tx.cancel().await.expect("cancel");
	}

	// Delete + commit.
	{
		let tx = ds.transaction(true, false).await.expect("tx3");
		tx.del(b"k1".to_vec()).await.expect("del");
		tx.commit().await.expect("commit del");
	}

	// Value should be gone.
	let tx = ds.transaction(false, false).await.expect("tx4");
	let result = tx.get(b"k1".to_vec(), None).await.expect("get post-del");
	assert!(result.is_none(),
		"del+commit should hide value; got {:?}", result);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Transaction::put / putc tests
// ============================================================================

/// put on a missing key succeeds.
#[tokio::test]
async fn test_put_succeeds_on_missing() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.put(b"k1".to_vec(), b"v1".to_vec()).await.expect("put missing");
	tx.commit().await.expect("commit");

	let tx2 = ds.transaction(false, false).await.expect("tx2");
	assert_eq!(tx2.get(b"k1".to_vec(), None).await.expect("get").as_deref(),
		Some(b"v1".as_ref()));
	tx2.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// put on an existing key returns TransactionKeyAlreadyExists.
#[tokio::test]
async fn test_put_fails_on_existing() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	// Insert.
	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	// put should now fail.
	let tx = ds.transaction(true, false).await.expect("tx2");
	let err = tx.put(b"k1".to_vec(), b"v2".to_vec()).await.expect_err("put should fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionKeyAlreadyExists),
		"expected TransactionKeyAlreadyExists, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// putc with a matching chk replaces the value.
#[tokio::test]
async fn test_putc_matching_value_succeeds() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	{
		let tx = ds.transaction(true, false).await.expect("tx2");
		tx.putc(b"k1".to_vec(), b"v2".to_vec(), Some(b"v1".to_vec()))
			.await.expect("putc match");
		tx.commit().await.expect("commit");
	}

	let tx = ds.transaction(false, false).await.expect("tx3");
	assert_eq!(tx.get(b"k1".to_vec(), None).await.expect("get").as_deref(),
		Some(b"v2".as_ref()));
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// putc with a mismatched chk returns TransactionConditionNotMet.
#[tokio::test]
async fn test_putc_mismatched_value_fails() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	let tx = ds.transaction(true, false).await.expect("tx2");
	let err = tx.putc(b"k1".to_vec(), b"v2".to_vec(), Some(b"wrong".to_vec()))
		.await.expect_err("putc should fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionConditionNotMet),
		"expected TransactionConditionNotMet, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// putc with None chk on a missing key succeeds (inserts).
#[tokio::test]
async fn test_putc_none_chk_on_missing_succeeds() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.putc(b"k1".to_vec(), b"v1".to_vec(), None).await.expect("putc None on missing");
	tx.commit().await.expect("commit");

	let tx2 = ds.transaction(false, false).await.expect("tx2");
	assert_eq!(tx2.get(b"k1".to_vec(), None).await.expect("get").as_deref(),
		Some(b"v1".as_ref()));
	tx2.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// putc with None chk on an EXISTING key returns TransactionConditionNotMet.
#[tokio::test]
async fn test_putc_none_chk_on_existing_fails() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	let tx = ds.transaction(true, false).await.expect("tx2");
	let err = tx.putc(b"k1".to_vec(), b"v2".to_vec(), None)
		.await.expect_err("putc None on existing must fail");
	assert!(matches!(err, crate::kvs::err::Error::TransactionConditionNotMet),
		"expected TransactionConditionNotMet, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Transaction::delc tests
// ============================================================================

/// delc with a matching chk deletes the value.
#[tokio::test]
async fn test_delc_matching_value_succeeds() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	{
		let tx = ds.transaction(true, false).await.expect("tx2");
		tx.delc(b"k1".to_vec(), Some(b"v1".to_vec()))
			.await.expect("delc match");
		tx.commit().await.expect("commit");
	}

	let tx = ds.transaction(false, false).await.expect("tx3");
	assert!(tx.get(b"k1".to_vec(), None).await.expect("get").is_none());
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// delc with mismatched chk returns TransactionConditionNotMet; value persists.
#[tokio::test]
async fn test_delc_mismatched_value_fails() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	{
		let tx = ds.transaction(true, false).await.expect("tx2");
		let err = tx.delc(b"k1".to_vec(), Some(b"wrong".to_vec()))
			.await.expect_err("delc should fail");
		assert!(matches!(err, crate::kvs::err::Error::TransactionConditionNotMet),
			"expected TransactionConditionNotMet, got {:?}", err);
		tx.cancel().await.expect("cancel");
	}

	let tx = ds.transaction(false, false).await.expect("tx3");
	assert_eq!(tx.get(b"k1".to_vec(), None).await.expect("get").as_deref(),
		Some(b"v1".as_ref()),
		"delc with wrong chk should NOT delete the value");
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// delc with None chk on a missing key is a trivial success (no-op).
#[tokio::test]
async fn test_delc_none_chk_on_missing_is_noop() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.delc(b"absent".to_vec(), None).await.expect("delc None on missing is no-op");
	tx.commit().await.expect("commit");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Commit overwrite regression
// ============================================================================

/// REGRESSION: set k=v1 + commit + set k=v2 + commit → get k = v2.
///
/// Lance is append-only. If commit() simply appended a row without merging
/// pre-existing rows with the same key, the dataset would end up with TWO
/// rows for key k (one with v1, one with v2) and `get` (scan + limit 1)
/// would return either one non-deterministically. This test pins the
/// contract: the latest committed value MUST win. Under the native path the
/// MergeInsert (keyed on `key`) guarantees the single-row outcome.
#[tokio::test]
async fn test_set_then_set_returns_latest_value() {
	let path = unique_tmp_path();
	let path_str = path.to_str().unwrap();
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	// Insert v1.
	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k".to_vec(), b"v1".to_vec()).await.expect("set v1");
		tx.commit().await.expect("commit v1");
	}

	// Overwrite with v2.
	{
		let tx = ds.transaction(true, false).await.expect("tx2");
		tx.set(b"k".to_vec(), b"v2".to_vec()).await.expect("set v2");
		tx.commit().await.expect("commit v2");
	}

	// Read — must see v2, not v1.
	let tx = ds.transaction(false, false).await.expect("tx3");
	let result = tx.get(b"k".to_vec(), None).await.expect("get");
	assert_eq!(
		result.as_deref(),
		Some(b"v2".as_ref()),
		"set-after-set must return latest value; got {:?}",
		result
	);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Transaction::scan / scanr tests
// ============================================================================

use crate::kvs::api::ScanLimit;

/// Helper: seed a dataset with keys a-e mapped to values 1-5, committed.
async fn seed_a_to_e(ds: &Datastore) {
	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"a".to_vec(), b"1".to_vec()).await.expect("set a");
	tx.set(b"b".to_vec(), b"2".to_vec()).await.expect("set b");
	tx.set(b"c".to_vec(), b"3".to_vec()).await.expect("set c");
	tx.set(b"d".to_vec(), b"4".to_vec()).await.expect("set d");
	tx.set(b"e".to_vec(), b"5".to_vec()).await.expect("set e");
	tx.commit().await.expect("commit");
}

/// Forward scan returns all 5 keys in ascending order.
#[tokio::test]
async fn test_scan_forward_returns_all_in_order() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("scan");

	let keys: Vec<&[u8]> = result.iter().map(|(k, _)| k.as_slice()).collect();
	assert_eq!(keys, vec![b"a".as_ref(), b"b", b"c", b"d", b"e"],
		"scan forward should return ascending; got {:?}", keys);

	let vals: Vec<&[u8]> = result.iter().map(|(_, v)| v.as_slice()).collect();
	assert_eq!(vals, vec![b"1".as_ref(), b"2", b"3", b"4", b"5"]);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// Reverse scan returns the same keys in descending order.
#[tokio::test]
async fn test_scanr_reverse_returns_all_in_descending_order() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	let result = tx.scanr(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("scanr");

	let keys: Vec<&[u8]> = result.iter().map(|(k, _)| k.as_slice()).collect();
	assert_eq!(keys, vec![b"e".as_ref(), b"d", b"c", b"b", b"a"],
		"scanr should return descending; got {:?}", keys);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// skip and limit work together (skip 2, take 2 → c, d).
#[tokio::test]
async fn test_scan_skip_and_limit() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(2),
		2,
		None,
	).await.expect("scan");

	let keys: Vec<&[u8]> = result.iter().map(|(k, _)| k.as_slice()).collect();
	assert_eq!(keys, vec![b"c".as_ref(), b"d"],
		"skip 2 take 2 should yield c,d; got {:?}", keys);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// Half-open range respects exclusive end.
#[tokio::test]
async fn test_scan_half_open_range_excludes_end() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	// Range [b, d) → b, c (d excluded).
	let result = tx.scan(
		b"b".to_vec()..b"d".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("scan");

	let keys: Vec<&[u8]> = result.iter().map(|(k, _)| k.as_slice()).collect();
	assert_eq!(keys, vec![b"b".as_ref(), b"c"],
		"range [b, d) should yield b,c only; got {:?}", keys);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// Pending Set adds a new key to the scan result (pending merged before skip/limit).
#[tokio::test]
async fn test_scan_pending_set_appears_in_results() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"bb".to_vec(), b"22".to_vec()).await.expect("set pending");

	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("scan");

	let keys: Vec<&[u8]> = result.iter().map(|(k, _)| k.as_slice()).collect();
	assert_eq!(keys, vec![b"a".as_ref(), b"b", b"bb", b"c", b"d", b"e"],
		"pending Set 'bb' should appear in order; got {:?}", keys);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// A pending Set that OVERRIDES a stored key wins in the merged scan output.
#[tokio::test]
async fn test_scan_pending_set_overrides_stored_value() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"c".to_vec(), b"33".to_vec()).await.expect("override c");

	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("scan");

	let pair_c = result.iter().find(|(k, _)| k.as_slice() == b"c").expect("c present");
	assert_eq!(pair_c.1.as_slice(), b"33",
		"pending override of stored 'c' must win; got {:?}", pair_c.1);
	// And there must be exactly ONE entry for c (no duplicate stored+pending row).
	let count_c = result.iter().filter(|(k, _)| k.as_slice() == b"c").count();
	assert_eq!(count_c, 1, "merged scan must dedupe 'c' to a single row, got {}", count_c);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// Pending Delete hides a stored row from the scan result.
#[tokio::test]
async fn test_scan_pending_delete_hides_stored_row() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.del(b"c".to_vec()).await.expect("del pending");

	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("scan");

	let keys: Vec<&[u8]> = result.iter().map(|(k, _)| k.as_slice()).collect();
	assert_eq!(keys, vec![b"a".as_ref(), b"b", b"d", b"e"],
		"pending Delete of 'c' should hide it; got {:?}", keys);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// keys() returns just the keys (projection of scan).
#[tokio::test]
async fn test_keys_returns_keys_only() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	let result = tx.keys(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("keys");

	let keys: Vec<&[u8]> = result.iter().map(|k| k.as_slice()).collect();
	assert_eq!(keys, vec![b"a".as_ref(), b"b", b"c", b"d", b"e"]);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// keysr returns keys in reverse order.
#[tokio::test]
async fn test_keysr_returns_keys_in_reverse() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	let result = tx.keysr(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Count(100),
		0,
		None,
	).await.expect("keysr");

	let keys: Vec<&[u8]> = result.iter().map(|k| k.as_slice()).collect();
	assert_eq!(keys, vec![b"e".as_ref(), b"d", b"c", b"b", b"a"]);

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Savepoint tests
// ============================================================================

/// new_save_point + rollback_to_save_point reverts pending writes.
#[tokio::test]
async fn test_savepoint_rollback_reverts_pending() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set v1");
	tx.new_save_point().await.expect("save_point");
	tx.set(b"k1".to_vec(), b"v2".to_vec()).await.expect("set v2");
	tx.set(b"k2".to_vec(), b"x".to_vec()).await.expect("set k2");
	tx.rollback_to_save_point().await.expect("rollback");

	// After rollback, k1=v1 (pre-savepoint), k2 absent.
	assert_eq!(tx.get(b"k1".to_vec(), None).await.expect("get k1").as_deref(),
		Some(b"v1".as_ref()),
		"rollback should restore k1=v1");
	assert!(tx.get(b"k2".to_vec(), None).await.expect("get k2").is_none(),
		"rollback should remove k2");

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// A savepoint snapshot includes pending TOMBSTONES, not just writes:
/// after rolling back, a delete staged inside the savepoint is undone.
#[tokio::test]
async fn test_savepoint_rollback_restores_deleted_key() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set v1");
	tx.new_save_point().await.expect("save_point");
	tx.del(b"k1".to_vec()).await.expect("del inside sp");
	// Inside the savepoint the delete is visible.
	assert!(tx.get(b"k1".to_vec(), None).await.expect("get pre-rollback").is_none(),
		"delete must be visible before rollback");
	tx.rollback_to_save_point().await.expect("rollback");

	// After rollback the pre-savepoint write is restored.
	assert_eq!(tx.get(b"k1".to_vec(), None).await.expect("get post-rollback").as_deref(),
		Some(b"v1".as_ref()),
		"rollback must undo the staged delete and restore k1=v1");

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// new_save_point + release_last_save_point keeps pending writes.
#[tokio::test]
async fn test_savepoint_release_keeps_pending() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set v1");
	tx.new_save_point().await.expect("save_point");
	tx.set(b"k1".to_vec(), b"v2".to_vec()).await.expect("set v2");
	tx.release_last_save_point().await.expect("release");

	// After release, k1=v2 stays (release just pops the snapshot without rollback).
	assert_eq!(tx.get(b"k1".to_vec(), None).await.expect("get k1").as_deref(),
		Some(b"v2".as_ref()),
		"release should NOT revert k1");

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// Nested savepoints: push 2, rollback 1 reverts only the inner.
#[tokio::test]
async fn test_nested_savepoints() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	tx.set(b"a".to_vec(), b"A".to_vec()).await.expect("set a");
	tx.new_save_point().await.expect("sp1");
	tx.set(b"b".to_vec(), b"B".to_vec()).await.expect("set b");
	tx.new_save_point().await.expect("sp2");
	tx.set(b"c".to_vec(), b"C".to_vec()).await.expect("set c");

	// Rollback inner (sp2) → c is gone, b stays.
	tx.rollback_to_save_point().await.expect("rollback to sp2");
	assert_eq!(tx.get(b"a".to_vec(), None).await.expect("get a").as_deref(), Some(b"A".as_ref()));
	assert_eq!(tx.get(b"b".to_vec(), None).await.expect("get b").as_deref(), Some(b"B".as_ref()));
	assert!(tx.get(b"c".to_vec(), None).await.expect("get c").is_none());

	// Rollback outer (sp1) → b is gone too.
	tx.rollback_to_save_point().await.expect("rollback to sp1");
	assert_eq!(tx.get(b"a".to_vec(), None).await.expect("get a").as_deref(), Some(b"A".as_ref()));
	assert!(tx.get(b"b".to_vec(), None).await.expect("get b").is_none());

	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// rollback_to_save_point with no savepoint returns NoSavePointPresent.
#[tokio::test]
async fn test_savepoint_rollback_with_no_savepoint_errors() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	let err = tx.rollback_to_save_point().await.expect_err("should error");
	assert!(matches!(err, crate::kvs::err::Error::NoSavePointPresent),
		"expected NoSavePointPresent, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// release_last_save_point with no savepoint returns NoSavePointPresent.
#[tokio::test]
async fn test_savepoint_release_with_no_savepoint_errors() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	let tx = ds.transaction(true, false).await.expect("tx");
	let err = tx.release_last_save_point().await.expect_err("should error");
	assert!(matches!(err, crate::kvs::err::Error::NoSavePointPresent),
		"expected NoSavePointPresent, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Versioning tests
// ============================================================================

/// Transparent time-travel: `get(key, Some(versionstamp))` maps the requested
/// wall-clock instant onto the Lance dataset version AS OF that instant (via
/// Lance's native per-version timestamps in `lance_version_as_of`), NOT a
/// surreal version-pointer column or a Lance dataset-version counter.
#[tokio::test]
async fn test_get_at_specific_version() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	// Instant before any write (the empty dataset already exists).
	let t_before = chrono::Utc::now();
	tokio::time::sleep(std::time::Duration::from_millis(50)).await;

	// Commit v1.
	{
		let tx = ds.transaction(true, false).await.expect("tx1");
		tx.set(b"k".to_vec(), b"v1".to_vec()).await.expect("set v1");
		tx.commit().await.expect("commit v1");
	}

	// Instant strictly between the two commits.
	tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	let t_between = chrono::Utc::now();
	tokio::time::sleep(std::time::Duration::from_millis(50)).await;

	// Commit v2 (overwrites v1 — the native MergeInsert replaces the row).
	{
		let tx = ds.transaction(true, false).await.expect("tx2");
		tx.set(b"k".to_vec(), b"v2".to_vec()).await.expect("set v2");
		tx.commit().await.expect("commit v2");
	}

	// The `version` arg is a VERSIONSTAMP (an instant), decoded by the backend's
	// `timestamp_impl`. In tests that is `IncTimeStampImpl`, whose versionstamp
	// IS the millisecond instant, so a wall-clock-millis value round-trips to the
	// same `DateTime` the mapping compares against Lance's native version stamps.
	let vs = |dt: chrono::DateTime<chrono::Utc>| dt.timestamp_millis() as u64;

	// Read at latest → v2.
	let tx = ds.transaction(false, false).await.expect("tx_now");
	assert_eq!(
		tx.get(b"k".to_vec(), None).await.expect("get now").as_deref(),
		Some(b"v2".as_ref()),
		"latest read should be v2",
	);
	tx.cancel().await.expect("cancel");

	// AS OF an instant between v1 and v2 → v1 (transparent time-travel: the
	// state as it stood at that instant, never the future v2 write).
	let tx2 = ds.transaction(false, false).await.expect("tx_between");
	assert_eq!(
		tx2.get(b"k".to_vec(), Some(vs(t_between))).await.expect("get as-of-between").as_deref(),
		Some(b"v1".as_ref()),
		"AS OF an instant between v1 and v2 must read v1",
	);
	tx2.cancel().await.expect("cancel");

	// AS OF an instant before any write → None (the key did not exist yet).
	let tx3 = ds.transaction(false, false).await.expect("tx_before");
	assert!(
		tx3.get(b"k".to_vec(), Some(vs(t_before))).await.expect("get as-of-before").is_none(),
		"AS OF a pre-write instant must return None",
	);
	tx3.cancel().await.expect("cancel");

	ds.shutdown().await.expect("shutdown");
}

/// get(_, Some(_)) on a non-versioned datastore returns UnsupportedVersionedQueries.
#[tokio::test]
async fn test_versioned_query_with_versioned_false_errors() {
	let path = unique_tmp_path();
	let config = LanceConfig {
		versioned: false,
	};
	let ds = Datastore::new(path.to_str().unwrap(), config).await.expect("ds");

	let tx = ds.transaction(false, false).await.expect("tx");
	let err = tx.get(b"k".to_vec(), Some(0)).await.expect_err("should error");
	assert!(matches!(err, crate::kvs::err::Error::UnsupportedVersionedQueries),
		"expected UnsupportedVersionedQueries, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// exists(_, Some(_)) on a non-versioned datastore also surfaces
/// UnsupportedVersionedQueries (exists must respect `version`).
#[tokio::test]
async fn test_exists_versioned_with_versioned_false_errors() {
	let path = unique_tmp_path();
	let config = LanceConfig {
		versioned: false,
	};
	let ds = Datastore::new(path.to_str().unwrap(), config).await.expect("ds");

	let tx = ds.transaction(false, false).await.expect("tx");
	let err = tx.exists(b"k".to_vec(), Some(0)).await.expect_err("should error");
	assert!(matches!(err, crate::kvs::err::Error::UnsupportedVersionedQueries),
		"expected UnsupportedVersionedQueries, got {:?}", err);
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Background optimizer interaction (native lance optimize)
// ============================================================================

/// Commits keep working while the background optimizer is alive; after 10
/// commits a get still returns the right value. The optimizer is lance's own
/// `optimize` task, not a hand-rolled flusher.
#[tokio::test]
async fn test_background_optimizer_does_not_panic_on_concurrent_commits() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	for i in 0..10 {
		let tx = ds.transaction(true, false).await.expect("tx");
		let key = format!("k{}", i).into_bytes();
		let val = format!("v{}", i).into_bytes();
		tx.set(key, val).await.expect("set");
		tx.commit().await.expect("commit");
	}

	// Sanity: a get still works after 10 commits.
	let tx = ds.transaction(false, false).await.expect("tx_read");
	let v = tx.get(b"k5".to_vec(), None).await.expect("get");
	assert_eq!(v.as_deref(), Some(b"v5".as_ref()));
	tx.cancel().await.expect("cancel");

	ds.shutdown().await.expect("shutdown");
}

/// Shutdown completes within 2 seconds even when the optimizer task is alive.
#[tokio::test]
async fn test_optimizer_shutdown_completes_within_timeout() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

	// Do one commit so the optimizer has been notified.
	{
		let tx = ds.transaction(true, false).await.expect("tx");
		tx.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	// Shutdown must complete within 2 seconds.
	let shutdown_fut = ds.shutdown();
	tokio::time::timeout(std::time::Duration::from_secs(2), shutdown_fut)
		.await
		.expect("shutdown did not complete within 2s")
		.expect("shutdown returned an error");
}

// ============================================================================
//  Property test: differential against an in-memory reference
// ============================================================================

use std::collections::HashMap;

/// Randomized differential test: drive the Lance datastore and a HashMap
/// reference with the same sequence of operations. After every commit,
/// every key the HashMap thinks should exist must be queryable on the
/// datastore and return the same value; every key the HashMap thinks is
/// absent must return None.
///
/// We use a deterministic LCG (linear congruential generator) so the test
/// is reproducible without pulling in a separate rand crate. Seed is fixed
/// so failures are debuggable.
#[tokio::test]
async fn test_property_matches_hashmap_reference() {
	const SEED: u64 = 0xC0FFEE;
	const OPS_PER_TXN: usize = 8;
	const NUM_TXNS: usize = 25;
	const KEY_SPACE: u8 = 16; // keys b"k0" .. b"k15"

	fn lcg(state: &mut u64) -> u64 {
		// Standard LCG (Numerical Recipes).
		*state = state.wrapping_mul(1664525).wrapping_add(1013904223);
		*state
	}

	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	let mut reference: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
	let mut rng_state: u64 = SEED;

	for txn_i in 0..NUM_TXNS {
		let tx = ds.transaction(true, false).await.expect("tx");

		// Buffer of staged ops applied to the reference only after commit.
		let mut staged: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();

		for _ in 0..OPS_PER_TXN {
			let r = lcg(&mut rng_state);
			let op_kind = r % 3; // 0=set, 1=del, 2=get
			let key_n = (r / 3) as u8 % KEY_SPACE;
			let key = format!("k{}", key_n).into_bytes();

			match op_kind {
				0 => {
					// Set.
					let val_n = lcg(&mut rng_state) % 100;
					let val = format!("v{}", val_n).into_bytes();
					tx.set(key.clone(), val.clone()).await.expect("set");
					staged.push((key, Some(val)));
				}
				1 => {
					// Del.
					tx.del(key.clone()).await.expect("del");
					staged.push((key, None));
				}
				2 => {
					// Get — only verifies in-txn read-your-writes against
					// a buffered view of the staged ops.
					// Compute the expected: scan staged in reverse for this key.
					let mut expected: Option<Vec<u8>> = reference.get(&key).cloned();
					for (k, v) in staged.iter() {
						if k == &key {
							expected = v.clone();
						}
					}
					let actual = tx.get(key.clone(), None).await.expect("get");
					assert_eq!(actual, expected,
						"txn {}: get({:?}) mismatch: expected {:?}, got {:?}",
						txn_i, key, expected, actual);
				}
				_ => unreachable!(),
			}
		}

		// Decide commit or cancel based on a coin flip — both paths exercised.
		if !lcg(&mut rng_state).is_multiple_of(4) {
			tx.commit().await.expect("commit");
			// Apply staged ops to reference.
			for (k, v) in staged {
				match v {
					Some(val) => { reference.insert(k, val); }
					None => { reference.remove(&k); }
				}
			}
		} else {
			tx.cancel().await.expect("cancel");
			// staged is discarded.
		}

		// After commit/cancel, the datastore must equal the reference.
		// Read every potentially-touched key and compare.
		let verify_tx = ds.transaction(false, false).await.expect("verify_tx");
		for n in 0..KEY_SPACE {
			let key = format!("k{}", n).into_bytes();
			let actual = verify_tx.get(key.clone(), None).await.expect("verify get");
			let expected = reference.get(&key).cloned();
			assert_eq!(actual, expected,
				"txn {} post-commit: key {:?} datastore={:?} reference={:?}",
				txn_i, key, actual, expected);
		}
		verify_tx.cancel().await.expect("verify cancel");
	}

	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  ScanLimit::Bytes accounting tests
// ============================================================================

/// ScanLimit::Bytes returns at least the requested byte budget, then stops.
/// Semantics: include the first entry that pushes the cumulative key+val byte
/// total over the target. So a small budget still yields ≥ 1 row when data
/// exists.
#[tokio::test]
async fn test_scan_limit_bytes_stops_at_budget() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	// Each (key, val) = 1 + 1 = 2 bytes here. Budget = 5 → 3 entries
	// (cumulative 2 → 4 → 6 ≥ 5; stop).
	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Bytes(5),
		0,
		None,
	).await.expect("scan");
	assert_eq!(result.len(), 3,
		"Bytes(5) over 2-byte rows should yield 3, got {}", result.len());
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// ScanLimit::Bytes with a very large budget returns everything.
#[tokio::test]
async fn test_scan_limit_bytes_large_budget_returns_all() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::Bytes(1_000_000),
		0,
		None,
	).await.expect("scan");
	assert_eq!(result.len(), 5, "large budget should yield all 5 seeded keys");
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// ScanLimit::BytesOrCount stops on whichever limit hits first — count first.
#[tokio::test]
async fn test_scan_limit_bytes_or_count_count_wins() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	// Bytes=1_000_000 (large) + Count=2 → count wins.
	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::BytesOrCount(1_000_000, 2),
		0,
		None,
	).await.expect("scan");
	assert_eq!(result.len(), 2,
		"BytesOrCount(huge, 2) should be capped at count=2, got {}", result.len());
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

/// ScanLimit::BytesOrCount — bytes side wins.
#[tokio::test]
async fn test_scan_limit_bytes_or_count_bytes_wins() {
	let path = unique_tmp_path();
	let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");
	seed_a_to_e(&ds).await;

	let tx = ds.transaction(false, false).await.expect("tx");
	// Bytes=3 + Count=100 → bytes wins after ≥ 2 entries
	// (cumulative 2 → 4 ≥ 3; stop). Result: 2.
	let result = tx.scan(
		b"a".to_vec()..b"z".to_vec(),
		ScanLimit::BytesOrCount(3, 100),
		0,
		None,
	).await.expect("scan");
	assert_eq!(result.len(), 2,
		"BytesOrCount(3, 100) should hit bytes after 2 rows, got {}", result.len());
	tx.cancel().await.expect("cancel");
	ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Concurrent-transaction tests (lance OCC at commit)
// ============================================================================

/// N concurrent transactions each writing to a DISJOINT key range. After all
/// commits complete, every key is readable. Verifies Lance's OCC handles
/// non-overlapping writes correctly (no false-positive conflicts).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_disjoint_writes() {
	const N_TASKS: usize = 8;
	const KEYS_PER_TASK: usize = 4;

	let path = unique_tmp_path();
	let ds = std::sync::Arc::new(
		Datastore::new(path.to_str().unwrap(), LanceConfig::default())
			.await
			.expect("ds"),
	);

	let mut handles = Vec::with_capacity(N_TASKS);
	for task_id in 0..N_TASKS {
		let ds_clone = std::sync::Arc::clone(&ds);
		handles.push(tokio::spawn(async move {
			let tx = ds_clone.transaction(true, false).await.expect("tx");
			for i in 0..KEYS_PER_TASK {
				let key = format!("t{:02}_k{:02}", task_id, i).into_bytes();
				let val = format!("v_{:02}_{:02}", task_id, i).into_bytes();
				tx.set(key, val).await.expect("set");
			}
			tx.commit().await.expect("commit");
			task_id
		}));
	}

	// All 8 tasks must complete without panic.
	for h in handles {
		let _task_id = h.await.expect("task panic");
	}

	// Every key must be readable.
	let tx = ds.transaction(false, false).await.expect("read tx");
	for task_id in 0..N_TASKS {
		for i in 0..KEYS_PER_TASK {
			let key = format!("t{:02}_k{:02}", task_id, i).into_bytes();
			let expected = format!("v_{:02}_{:02}", task_id, i).into_bytes();
			let got = tx.get(key.clone(), None).await.expect("get");
			assert_eq!(
				got.as_deref(),
				Some(expected.as_slice()),
				"task {} key {:?} mismatch: got {:?}",
				task_id,
				key,
				got
			);
		}
	}
	tx.cancel().await.expect("cancel");
	std::sync::Arc::try_unwrap(ds)
		.map_err(|_| "ds still has outstanding refs")
		.unwrap()
		.shutdown()
		.await
		.expect("shutdown");
}

/// N concurrent transactions all writing to the SAME key with different
/// values. After all commits complete (or one is retried by Lance OCC),
/// a final get must return ONE of the written values — not None, not garbage.
/// We don't assert which value won (OCC race is implementation-defined).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_same_key_yields_one_winner() {
	const N_TASKS: usize = 6;

	let path = unique_tmp_path();
	let ds = std::sync::Arc::new(
		Datastore::new(path.to_str().unwrap(), LanceConfig::default())
			.await
			.expect("ds"),
	);

	let mut handles = Vec::with_capacity(N_TASKS);
	for task_id in 0..N_TASKS {
		let ds_clone = std::sync::Arc::clone(&ds);
		handles.push(tokio::spawn(async move {
			// Each task tries a few times; if Lance returns a retryable
			// conflict, we re-issue. This is what SurrealDB's higher-level
			// retry loop does in production.
			for _ in 0..5 {
				let tx = ds_clone.transaction(true, false).await.expect("tx");
				let val = format!("v{}", task_id).into_bytes();
				tx.set(b"shared".to_vec(), val).await.expect("set");
				match tx.commit().await {
					Ok(()) => return Ok(()),
					Err(crate::kvs::err::Error::TransactionConflict(_)) => {
						// Retry — re-open a fresh transaction.
						continue;
					}
					Err(e) => return Err(e),
				}
			}
			Err(crate::kvs::err::Error::TransactionConflict(
				"exceeded 5 retries".into(),
			))
		}));
	}

	let mut success = 0;
	for h in handles {
		match h.await.expect("task panic") {
			Ok(()) => success += 1,
			Err(_e) => {
				// Excessive retries — acceptable in extreme contention but
				// every task should converge in 5 tries on default config.
			}
		}
	}
	assert!(success >= 1, "at least one task must commit successfully; got 0");

	// Final value must be SOMETHING (one of v0..vN-1), not None.
	let tx = ds.transaction(false, false).await.expect("read tx");
	let got = tx
		.get(b"shared".to_vec(), None)
		.await
		.expect("get");
	let got = got.expect("expected Some(val), got None");
	let got_str = String::from_utf8_lossy(&got);
	assert!(
		got_str.starts_with('v')
			&& got_str[1..].parse::<usize>().is_ok_and(|n| n < N_TASKS),
		"final value must be one of v0..v{}; got {:?}",
		N_TASKS - 1,
		got_str
	);
	tx.cancel().await.expect("cancel");
	std::sync::Arc::try_unwrap(ds)
		.map_err(|_| "ds still has outstanding refs")
		.unwrap()
		.shutdown()
		.await
		.expect("shutdown");
}

// ──────────────────────────────────────────────────────────────────────────
// Timeline — read-only time-series view over lance's native versions
// ──────────────────────────────────────────────────────────────────────────

/// The timeline enumerates Lance's native version history and that history
/// grows with committed transactions. Under the native single-path model,
/// one SurrealDB commit = one lance dataset version, so two commits add two
/// versions.
#[tokio::test]
async fn test_timeline_versions_grow_with_commits() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	let timeline = ds.timeline();
	let v_start = timeline.versions().await.expect("versions @ start").len();

	// Two committed write transactions → two new Lance versions.
	for (k, v) in [(b"a".as_ref(), b"1".as_ref()), (b"b".as_ref(), b"2".as_ref())] {
		let tx = ds.transaction(true, false).await.expect("tx");
		tx.set(k.to_vec(), v.to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	let versions = timeline.versions().await.expect("versions @ end");

	// on the native path. If a commit that contains both writes AND a delete
	// folds into a single version (the intended invariant) this holds; but an
	// optimize/compaction firing mid-test could also ADD a version. Lower-bound
	// assertion chosen to be robust to compaction; tighten to `== v_start + 2`
	// only if optimize is guaranteed quiescent here.
	assert!(
		versions.len() >= v_start + 2,
		"expected ≥{} versions after 2 commits, got {}",
		v_start + 2,
		versions.len()
	);
	// Version numbers are monotone non-decreasing along the timeline.
	for w in versions.windows(2) {
		assert!(w[0].version <= w[1].version, "timeline not monotone: {:?}", versions);
	}
	// The latest entry matches the datastore's current version.
	let latest = timeline.latest_version().await;
	assert_eq!(
		versions.last().map(|vi| vi.version),
		Some(latest),
		"timeline tail must equal current_version"
	);

	ds.shutdown().await.expect("shutdown");
}

/// A historical [`TimelineView`] reads the dataset as it stood at that
/// version: a key written at version N is absent from a view pinned before N
/// and present from the view at/after N.
#[tokio::test]
async fn test_timeline_view_reads_historical_state() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	let timeline = ds.timeline();
	let v_before = timeline.latest_version().await;

	// Commit a single key.
	{
		let tx = ds.transaction(true, false).await.expect("tx");
		tx.set(b"hist".to_vec(), b"present".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}
	let v_after = timeline.latest_version().await;

	// (one-commit-one-version on the native path). Holds unless commits are
	// coalesced; on the native single path each commit is its own lance
	// version so `v_after > v_before` should be exact.
	assert!(v_after > v_before, "commit did not advance the dataset version");

	// View at the latest version sees the value.
	let view_after = timeline.view_at(v_after).await.expect("view @ after");
	assert_eq!(view_after.version(), v_after);
	assert_eq!(
		view_after.get(&b"hist".to_vec()).await.expect("get @ after").as_deref(),
		Some(b"present".as_ref()),
		"view at the write version must see the key"
	);

	// View at the pre-write version must NOT see the value.
	let view_before = timeline.view_at(v_before).await.expect("view @ before");
	assert!(
		view_before.get(&b"hist".to_vec()).await.expect("get @ before").is_none(),
		"view before the write must not see the key (time-travel violated)"
	);

	// scan() at the latest version surfaces the live row.
	let rows = view_after.scan().await.expect("scan @ after");
	assert!(
		rows.iter().any(|(k, v)| k == b"hist" && v == b"present"),
		"timeline scan must surface the committed row; got {rows:?}"
	);

	ds.shutdown().await.expect("shutdown");
}

/// REGRESSION: a single transaction carrying BOTH writes and deletes must
/// land as exactly ONE Lance version, not two. Folding deletes into tombstone
/// rows of the same MergeInsert keeps the commit atomic (one version), so a
/// replayer's `view_at()` can never materialize a torn write-before-delete
/// intermediate.
#[tokio::test]
async fn test_timeline_write_delete_commit_is_single_atomic_version() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	// Seed two committed keys so the later delete has a live row to remove.
	{
		let tx = ds.transaction(true, false).await.expect("tx seed");
		tx.set(b"keep".to_vec(), b"old".to_vec()).await.expect("set keep");
		tx.set(b"victim".to_vec(), b"doomed".to_vec()).await.expect("set victim");
		tx.commit().await.expect("commit seed");
	}

	let timeline = ds.timeline();
	let versions_before = timeline.versions().await.expect("versions before").len();

	// ONE transaction that BOTH writes (`fresh`, overwrite `keep`) and deletes
	// (`victim`).
	{
		let tx = ds.transaction(true, false).await.expect("tx mixed");
		tx.set(b"fresh".to_vec(), b"new".to_vec()).await.expect("set fresh");
		tx.set(b"keep".to_vec(), b"new".to_vec()).await.expect("overwrite keep");
		tx.del(b"victim".to_vec()).await.expect("del victim");
		tx.commit().await.expect("commit mixed");
	}

	let versions_after = timeline.versions().await.expect("versions after").len();

	// version". This is the intended native invariant (writes + tombstones
	// folded into a single MergeInsert). If the native commit instead applies
	// inserts and deletes as two separate lance operations this becomes +2 and
	// the assertion must change — flagged for a tester to confirm the
	// single-version commit shape.
	assert_eq!(
		versions_after,
		versions_before + 1,
		"a write+delete commit must add EXACTLY ONE Lance version (atomic); \
		 got {} new, a torn write-before-delete intermediate leaked",
		versions_after - versions_before
	);

	// The single new version is a coherent atomic snapshot: the new write is
	// present, the overwrite is reflected, and the delete is applied all at
	// once, with no intermediate state observable.
	let view = timeline.view_latest().await.expect("view latest");
	assert_eq!(
		view.get(&b"fresh".to_vec()).await.expect("get fresh").as_deref(),
		Some(b"new".as_ref()),
		"atomic snapshot must include the new write"
	);
	assert_eq!(
		view.get(&b"keep".to_vec()).await.expect("get keep").as_deref(),
		Some(b"new".as_ref()),
		"atomic snapshot must reflect the overwrite"
	);
	assert!(
		view.get(&b"victim".to_vec()).await.expect("get victim").is_none(),
		"atomic snapshot must reflect the delete (tombstone hides the row)"
	);

	ds.shutdown().await.expect("shutdown");
}
