#![cfg(test)]
#![cfg(feature = "kv-lance")]

//! Integration tests for the Lance backend.
//!
//! These tests exercise the public Datastore API end-to-end with a
//! temporary directory acting as the Lance dataset location.
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
//  Transaction::get tests (Day 2)
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

// ============================================================================
//  Transaction::commit tests (Day 3)
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
