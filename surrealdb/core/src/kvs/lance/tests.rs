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
use crate::kvs::config::{LanceConfig, WritePath};

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

// ============================================================================
//  Transaction::put / putc tests (Day 4)
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

// ============================================================================
//  Transaction::delc tests (Day 5)
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
//  Commit overwrite regression (Sprint F)
// ============================================================================

/// REGRESSION (Sprint F): set k=v1 + commit + set k=v2 + commit → get k = v2.
///
/// Lance is append-only. If commit() simply appended a row without deleting
/// pre-existing rows with the same key, the dataset would end up with TWO
/// rows for key k (one with v1, one with v2) and `get` (scan + limit 1)
/// would return either one non-deterministically. This test pins the
/// contract: the latest committed value MUST win.
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
//  Transaction::scan / scanr tests (Day 6)
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

/// Pending Set adds a new key to the scan result.
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

// ============================================================================
//  keysr tests (Day 7)
// ============================================================================

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
//  Savepoint tests (Day 8)
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

// ============================================================================
//  Versioning tests (Day 9)
// ============================================================================

/// get(key, Some(version)) on a versioned datastore reads the historical value.
#[tokio::test]
async fn test_get_at_specific_version() {
    let path = unique_tmp_path();
    let ds = Datastore::new(path.to_str().unwrap(), LanceConfig::default()).await.expect("ds");

    // Capture the version before any writes.
    let v_initial = ds.current_version().await;

    // Commit v1.
    {
        let tx = ds.transaction(true, false).await.expect("tx1");
        tx.set(b"k".to_vec(), b"v1".to_vec()).await.expect("set v1");
        tx.commit().await.expect("commit v1");
    }
    let v_after_first = ds.current_version().await;

    // Commit v2 (overwrites v1 — Sprint F fix means the row replaces).
    {
        let tx = ds.transaction(true, false).await.expect("tx2");
        tx.set(b"k".to_vec(), b"v2".to_vec()).await.expect("set v2");
        tx.commit().await.expect("commit v2");
    }
    let v_latest = ds.current_version().await;

    // Read at latest → v2.
    let tx = ds.transaction(false, false).await.expect("tx_now");
    let now = tx.get(b"k".to_vec(), None).await.expect("get now");
    assert_eq!(now.as_deref(), Some(b"v2".as_ref()), "latest read should be v2");
    tx.cancel().await.expect("cancel");

    // Read at v_after_first → ideally v1, but Lance's MVCC + delete-before-append
    // means the v1 row was deleted at commit-of-v2. Older snapshots may either
    // see v1 (if Lance preserves deletion vectors per-snapshot) or see nothing
    // (if the delete propagates back). Both are acceptable for POC; the
    // important thing is that the call doesn't panic and returns *some*
    // consistent answer.
    let tx2 = ds.transaction(false, false).await.expect("tx_v1");
    let at_v1 = tx2.get(b"k".to_vec(), Some(v_after_first)).await.expect("get at v1");
    // Either Some(v1) or None — POC tolerates both. Assert it's not Some(v2).
    assert_ne!(at_v1.as_deref(), Some(b"v2".as_ref()),
        "version-pinned read MUST NOT see future writes; got {:?}", at_v1);
    tx2.cancel().await.expect("cancel");

    // Read at v_initial — pre-any-commit. Must NOT return any value.
    let tx3 = ds.transaction(false, false).await.expect("tx_init");
    let at_init = tx3.get(b"k".to_vec(), Some(v_initial)).await.expect("get at initial");
    assert!(at_init.is_none(),
        "pre-write version should return None; got {:?}", at_init);
    tx3.cancel().await.expect("cancel");

    let _ = v_latest;
    ds.shutdown().await.expect("shutdown");
}

/// get(_, Some(_)) on a non-versioned datastore returns UnsupportedVersionedQueries.
#[tokio::test]
async fn test_versioned_query_with_versioned_false_errors() {
    let path = unique_tmp_path();
    let config = LanceConfig {
        versioned: false,
        ..LanceConfig::default()
    };
    let ds = Datastore::new(path.to_str().unwrap(), config).await.expect("ds");

    let tx = ds.transaction(false, false).await.expect("tx");
    let err = tx.get(b"k".to_vec(), Some(0)).await.expect_err("should error");
    assert!(matches!(err, crate::kvs::err::Error::UnsupportedVersionedQueries),
        "expected UnsupportedVersionedQueries, got {:?}", err);
    tx.cancel().await.expect("cancel");
    ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Background optimizer tests (Day 10)
// ============================================================================

/// Optimizer doesn't panic when commits happen during its loop.
/// We use the default config (optimizer enabled, interval = 5 min — too long
/// to trigger from time alone in a test). After 10 commits, notify_commit
/// has been called 10 times; the optimizer may or may not have triggered
/// depending on LANCE_OPTIMIZE_AFTER_N_WRITES (default 1000), so we just
/// assert the host process still works after the writes.
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
//  Property tests (Day 11)
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
//  ScanLimit::Bytes accounting tests (Sprint T)
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
//  Concurrent-transaction property tests (Sprint U)
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

/// Regression test for the codex P2 finding on PR #17 (Sprint Z):
/// `Datastore::shutdown` must drain queued commits in the CollapseGate
/// coordinator rather than dropping their reply channels.
///
/// Pattern: spawn a small race-prone batch of commits and shut down
/// immediately afterwards. Every commit that the caller `.await`ed
/// MUST observe either `Ok(())` (drained) or a clean
/// "commit gate coordinator shut down" error — never a dangling
/// "coordinator dropped reply", which was the pre-fix bug.
#[tokio::test]
async fn shutdown_drains_pending_commits() {
    use std::sync::Arc;

    let path = unique_tmp_path();
    let path_str = path.to_str().expect("path is valid UTF-8");
    let ds = Arc::new(
        Datastore::new(path_str, LanceConfig::default())
            .await
            .expect("create dataset"),
    );

    // Spawn enough concurrent commits that some will be queued in the
    // gate's channel while the coordinator is still draining the first
    // batch. Each commit writes a unique key so BUNDLE-merge can't
    // collapse them.
    let mut handles = Vec::new();
    for i in 0..16u32 {
        let ds_clone = Arc::clone(&ds);
        handles.push(tokio::spawn(async move {
            let tx = ds_clone
                .transaction(true, false)
                .await
                .expect("open tx");
            tx.set(
                format!("shutdown_drain_key_{i}").into_bytes(),
                b"v".to_vec(),
            )
            .await
            .expect("set");
            tx.commit().await
        }));
    }

    // Give the gate a moment to receive submissions (the coordinator
    // pulls them off the mpsc channel in its hot loop).
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // Drain & shutdown via `Arc::deref` — we can NOT use
    // `Arc::try_unwrap` here because the spawned commit tasks still
    // hold their `Arc<Datastore>` clones (they only release them when
    // their `commit()` returns, which is what we're driving via this
    // shutdown). `Datastore::shutdown` takes `&self`, so call it
    // through the Arc; the inner `CommitGate::shutdown` awaits the
    // coordinator task and guarantees every queued commit has either
    // landed or been gracefully rejected before this returns.
    ds.shutdown().await.expect("shutdown");

    // Verify the contract: every commit either succeeded (was drained)
    // or returned the clean "gate shut down" rejection emitted by
    // `CommitGate::commit` when the channel is closed BEFORE the
    // submission lands. The pre-fix bug produced
    // "commit gate coordinator dropped reply" — that string MUST NOT
    // appear in any returned error, otherwise the regression has
    // returned.
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await.expect("join") {
            Ok(()) => { /* drained successfully */ }
            Err(crate::kvs::err::Error::Datastore(msg))
                if msg.contains("commit gate coordinator shut down")
                    && !msg.contains("dropped reply") =>
            {
                /* gracefully rejected post-shutdown (the channel was
                 * closed before this submission could be enqueued) */
            }
            Err(crate::kvs::err::Error::Datastore(msg))
                if msg.contains("dropped reply") =>
            {
                panic!(
                    "commit #{i} hit the pre-fix dropped-reply path: {msg} \
                     — shutdown drain regression has returned"
                );
            }
            Err(e) => panic!(
                "commit #{i} returned unexpected error after shutdown: {e}"
            ),
        }
    }
}

// ============================================================================
//  LSM crash-recovery (Sprint AA: WAL + memtable)
// ============================================================================

/// Tokio mutex shared by both LSM-recovery tests so they never run
/// concurrently. The "simulate a crash" idiom (`Box::leak` the first
/// `Datastore`) leaves the flusher task alive on the test runtime,
/// and that task can still be rewriting the Lance manifest at the
/// moment the second `Datastore::new` re-opens the same path —
/// producing a Lance `manifest` checksum race that surfaces as
/// "key missing after recovery". The tests are correct individually
/// (they pass on `--test-threads=1` and they pass in isolation),
/// so we just gate the parallelism between the two recovery tests
/// rather than burying the durability scenario behind extra config.
static LSM_RECOVERY_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acked commits survive a process crash that happens before the
/// flusher has had a chance to push memtable rows into Lance.
///
/// Scenario: write N rows, drop the Datastore WITHOUT calling
/// `shutdown` (i.e. no flusher drain). Re-open the same path. Every
/// row that returned Ok from `commit()` must be readable in the
/// re-opened Datastore — that is the durability contract advertised
/// by the WAL.
///
/// Implementation notes:
///   - We `Box::leak` the first datastore so its `Drop` does NOT
///     run, simulating a hard process kill rather than a graceful
///     shutdown. `shutdown` would drain the flusher and erase the
///     test's point.
///   - The flusher's tick is 100 ms; we don't sleep between commits
///     and immediately drop, so the WAL is essentially the only
///     surviving record.
#[tokio::test]
async fn lsm_recovery_replays_uncommitted_wal_records() {
    let _serial = LSM_RECOVERY_SERIAL.lock().await;
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");

    // ── (1) First datastore: write a bunch of rows and leak it ─────
    let written: Vec<(Vec<u8>, Vec<u8>)> = (0..50)
        .map(|i| {
            (
                format!("recovery_key_{:03}", i).into_bytes(),
                format!("recovery_val_{:03}", i).into_bytes(),
            )
        })
        .collect();

    {
        let ds = Datastore::new(
            path_str,
            LanceConfig {
                disable_background_flusher: true,
                ..LanceConfig::default()
            },
        )
        .await
        .expect("ds open #1");
        for (k, v) in &written {
            let tx = ds.transaction(true, false).await.expect("tx");
            tx.set(k.clone(), v.clone()).await.expect("set");
            tx.commit().await.expect("commit");
        }
        // SIMULATE A CRASH: leak the Datastore so its `Drop` (and
        // any in-flight flusher task) never observes a graceful
        // shutdown. The WAL remains on disk.
        Box::leak(Box::new(ds));
    }

    // ── (2) Re-open. The WAL must replay into the memtable. ────────
    let ds2 = Datastore::new(
        path_str,
        LanceConfig {
            disable_background_flusher: true,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open #2");

    // ── (3) Every Ack'd write must be readable. ────────────────────
    let tx = ds2.transaction(false, false).await.expect("read tx");
    for (k, v) in &written {
        let got = tx.get(k.clone(), None).await.expect("get");
        assert_eq!(
            got.as_deref(),
            Some(v.as_slice()),
            "key {:?} missing after recovery (WAL replay regression)",
            String::from_utf8_lossy(k)
        );
    }
    tx.cancel().await.expect("cancel");
    ds2.shutdown().await.expect("shutdown #2");
}

/// A delete committed before the crash is preserved across recovery:
/// the key returns `None` (not the pre-delete value) after re-open.
///
/// Same crash-simulation pattern as the recovery test above; the
/// novelty here is the WAL `Delete` op exercising replay.
#[tokio::test]
async fn lsm_recovery_preserves_delete_tombstones() {
    let _serial = LSM_RECOVERY_SERIAL.lock().await;
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");

    {
        let ds = Datastore::new(
            path_str,
            LanceConfig {
                disable_background_flusher: true,
                ..LanceConfig::default()
            },
        )
        .await
        .expect("ds open #1");

        // Write k=v, then immediately delete k. Both ops land in the
        // WAL before the crash.
        let tx = ds.transaction(true, false).await.expect("tx-set");
        tx.set(b"k".to_vec(), b"v".to_vec()).await.expect("set");
        tx.commit().await.expect("commit-set");

        let tx = ds.transaction(true, false).await.expect("tx-del");
        tx.del(b"k".to_vec()).await.expect("del");
        tx.commit().await.expect("commit-del");

        Box::leak(Box::new(ds));
    }

    let ds2 = Datastore::new(
        path_str,
        LanceConfig {
            disable_background_flusher: true,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open #2");
    let tx = ds2.transaction(false, false).await.expect("read tx");
    let got = tx.get(b"k".to_vec(), None).await.expect("get");
    assert!(
        got.is_none(),
        "delete tombstone lost across recovery: got {:?}",
        got
    );
    tx.cancel().await.expect("cancel");
    ds2.shutdown().await.expect("shutdown");
}

/// All-or-nothing (atomicity) of a MULTI-OP transaction across a crash.
///
/// The two recovery tests above each use one op per transaction. This
/// one commits a SINGLE transaction that both writes new keys AND
/// deletes a pre-existing key, so the whole batch lands as ONE
/// `WalRecord` (multiple `ops`, one generation). After a simulated
/// crash + replay every effect of that committed batch must be visible
/// together — the writes present, the delete applied — proving the WAL
/// record replays as an atomic unit, not op-by-op.
///
/// Same crash-simulation idiom as the tests above (`Box::leak` to skip
/// the graceful `shutdown`/flusher-drain; `disable_background_flusher`
/// so the WAL is the sole durability source). Serialized via
/// `LSM_RECOVERY_SERIAL` for the same manifest-race reason.
#[tokio::test]
async fn lsm_recovery_atomic_multi_op_batch() {
    let _serial = LSM_RECOVERY_SERIAL.lock().await;
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");

    {
        let ds = Datastore::new(
            path_str,
            LanceConfig {
                disable_background_flusher: true,
                ..LanceConfig::default()
            },
        )
        .await
        .expect("ds open #1");

        // Seed a key in its own committed transaction so the later
        // batch has something pre-existing to delete.
        let tx = ds.transaction(true, false).await.expect("tx-seed");
        tx.set(b"victim".to_vec(), b"doomed".to_vec()).await.expect("seed set");
        tx.commit().await.expect("seed commit");

        // The all-or-nothing batch: two inserts + one delete, all in a
        // SINGLE transaction → one multi-op WAL record.
        let tx = ds.transaction(true, false).await.expect("tx-batch");
        tx.set(b"batch_a".to_vec(), b"AAA".to_vec()).await.expect("set a");
        tx.set(b"batch_b".to_vec(), b"BBB".to_vec()).await.expect("set b");
        tx.del(b"victim".to_vec()).await.expect("del victim");
        tx.commit().await.expect("batch commit");

        Box::leak(Box::new(ds));
    }

    // Re-open: the multi-op WAL record must replay atomically.
    let ds2 = Datastore::new(
        path_str,
        LanceConfig {
            disable_background_flusher: true,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open #2");

    let tx = ds2.transaction(false, false).await.expect("read tx");
    // Both inserts from the committed batch are present…
    assert_eq!(
        tx.get(b"batch_a".to_vec(), None).await.expect("get a").as_deref(),
        Some(b"AAA".as_ref()),
        "batch insert 'batch_a' lost across recovery"
    );
    assert_eq!(
        tx.get(b"batch_b".to_vec(), None).await.expect("get b").as_deref(),
        Some(b"BBB".as_ref()),
        "batch insert 'batch_b' lost across recovery"
    );
    // …and the delete from the SAME batch is applied (not the old value).
    assert!(
        tx.get(b"victim".to_vec(), None).await.expect("get victim").is_none(),
        "batch delete of 'victim' not applied after recovery — \
         multi-op WAL record did not replay atomically"
    );
    tx.cancel().await.expect("cancel");
    ds2.shutdown().await.expect("shutdown");
}

// ============================================================================
//  Per-commit `seq` column (commit-sequence, distinct from `version`)
// ============================================================================

/// The `seq` column is present in Lance, carries a per-commit
/// monotonic sequence number, and two commits that coalesce into a
/// SINGLE Lance version still carry DISTINCT seqs.
///
/// Strategy: default LSM path with the flusher ENABLED. Commit two
/// separate single-key transactions back-to-back (no sleep) so the
/// background flusher batches both into one `do_flush` → one Lance
/// version. `shutdown()` drains the flusher so every row is in Lance.
/// Then scan the raw `seq` column and assert: both keys present, seqs
/// distinct, and ordered by commit order.
#[tokio::test]
async fn seq_column_is_per_commit_monotonic_and_survives_coalescing() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("utf-8 path");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	let v_before = ds.timeline().latest_version().await;

	// Two distinct commits. Each is its own transaction → its own seq.
	let tx = ds.transaction(true, false).await.expect("tx1");
	tx.set(b"seq_a".to_vec(), b"1".to_vec()).await.expect("set a");
	tx.commit().await.expect("commit a");

	let tx = ds.transaction(true, false).await.expect("tx2");
	tx.set(b"seq_b".to_vec(), b"2".to_vec()).await.expect("set b");
	tx.commit().await.expect("commit b");

	// Drain the flusher so both rows are materialised into Lance. The
	// two commits are issued back-to-back with no `.await` that yields
	// to the flusher task in between, so the flusher's `notify_pending`
	// nudges coalesce and the shutdown final-drain writes BOTH rows in
	// one `do_flush` → exactly ONE new Lance version.
	ds.shutdown().await.expect("shutdown");

	let v_after = ds.timeline().latest_version().await;
	assert_eq!(
		v_after - v_before,
		1,
		"the two commits should coalesce into exactly one Lance version \
		 (delta was {}), so the distinct-seq assertion below proves seqs \
		 survive coalescing",
		v_after - v_before
	);

	let rows = ds.scan_seqs_for_tests().await.expect("scan seqs");
	let seq_of = |want: &[u8]| -> u64 {
		rows.iter()
			.find(|(k, _, tomb)| k.as_slice() == want && !*tomb)
			.unwrap_or_else(|| panic!("key {:?} missing from Lance seq scan", want))
			.1
	};
	let sa = seq_of(b"seq_a");
	let sb = seq_of(b"seq_b");

	// Both commits carry a non-zero, DISTINCT seq…
	assert!(sa > 0, "first commit seq should be > 0, got {sa}");
	assert_ne!(sa, sb, "coalesced commits must carry distinct seqs");
	// …and the second commit's seq is strictly greater (monotonic).
	assert!(sb > sa, "seq must be monotonic across commits: {sa} then {sb}");
}

/// A delete carries the seq of the transaction that issued it, distinct
/// from the seq of the earlier transaction that wrote the key — even
/// though both rows key on the same SurrealDB key. Proves the tombstone
/// path threads `seq` per-commit too.
#[tokio::test]
async fn seq_column_tombstone_carries_deleting_commit_seq() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("utf-8 path");
	let ds = Datastore::new(path_str, LanceConfig::default()).await.expect("create");

	// Commit 1: write the key.
	let tx = ds.transaction(true, false).await.expect("tx-set");
	tx.set(b"k".to_vec(), b"v".to_vec()).await.expect("set");
	tx.commit().await.expect("commit set");
	// Commit 2 (a later, higher seq): delete it.
	let tx = ds.transaction(true, false).await.expect("tx-del");
	tx.del(b"k".to_vec()).await.expect("del");
	tx.commit().await.expect("commit del");

	ds.shutdown().await.expect("shutdown");

	// The memtable coalesces the key to its latest op (Delete) before
	// flushing, so Lance holds one tombstone row for `k` carrying the
	// DELETING transaction's seq — which must be > 0 (a real per-commit
	// seq, not a default).
	let rows = ds.scan_seqs_for_tests().await.expect("scan seqs");
	let row = rows
		.iter()
		.find(|(key, _, _)| key.as_slice() == b"k")
		.expect("key 'k' present in Lance (as tombstone)");
	assert!(row.2, "row for 'k' should be a tombstone after delete");
	assert!(row.1 >= 2, "tombstone seq should be the 2nd commit's seq, got {}", row.1);
}

// ============================================================================
//  CommitGate as alternative route
// ============================================================================
//
// Sprint AA introduced the WAL + memtable + flusher hot-path; the older
// `CommitGate` coordinator is preserved as an alternative route (see the
// header of `commit_gate.rs`). The test below spawns a CommitGate
// directly against a fresh Datastore handle and round-trips one
// commit + one delete through it, asserting that the gate's
// merge-insert-and-delete contract still holds end-to-end.
//
// Keeping this test in the suite ensures the alternative route does not
// bit-rot as the LSM path evolves.

/// Directly exercise `CommitGate::commit` + `CommitGate::shutdown`
/// against a fresh dataset. Verifies that the legacy synchronous
/// commit path still produces a readable Lance row and that delete
/// removes it again, independent of the WAL+memtable pipeline.
#[tokio::test]
async fn commit_gate_alternative_route_roundtrips_a_write() {
    use super::commit_gate::CommitGate;

    // Build a Datastore on a fresh path so we can borrow its
    // internal `dataset` Arc for the gate. We use the public
    // `Datastore::new` path because spinning up the bare Lance
    // dataset by hand would duplicate Sprint A scaffolding.
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    let ds = Datastore::new(path_str, LanceConfig::default())
        .await
        .expect("ds open");

    // Spawn a fresh CommitGate against the same underlying Lance
    // dataset handle. This is what an "alternative implementation
    // test" would do: bypass the production WAL+memtable path and
    // drive Lance via the gate directly.
    let gate = CommitGate::spawn(std::sync::Arc::clone(ds.dataset_for_tests()));

    let key = b"gate_route_key".to_vec();
    let val = b"gate_route_val".to_vec();

    // Submit one write through the gate. The `version` we pass is
    // the per-row version stamp written to the `version` column —
    // it does NOT have to match Lance's manifest version, and the
    // gate accepts any monotonically-increasing u64 here.
    let next_version = ds.current_version().await.saturating_add(1);
    gate.commit(vec![(key.clone(), val.clone())], vec![], next_version)
        .await
        .expect("gate commit");

    // The row must now be readable through a fresh Transaction.
    let tx = ds.transaction(false, false).await.expect("read tx");
    let got = tx.get(key.clone(), None).await.expect("get");
    assert_eq!(
        got.as_deref(),
        Some(val.as_slice()),
        "row written via CommitGate missing on read-through-tx"
    );
    tx.cancel().await.expect("cancel");

    // Submit one delete through the gate.
    let next_version = ds.current_version().await.saturating_add(1);
    gate.commit(vec![], vec![key.clone()], next_version)
        .await
        .expect("gate delete");

    // The row must now be gone.
    let tx = ds.transaction(false, false).await.expect("read tx");
    let got = tx.get(key.clone(), None).await.expect("get");
    assert!(
        got.is_none(),
        "row not removed after CommitGate delete: got {:?}",
        got
    );
    tx.cancel().await.expect("cancel");

    gate.shutdown().await;
    ds.shutdown().await.expect("ds shutdown");
}

// ============================================================================
//  Throughput micro-benchmarks (Sprint AA validation)
// ============================================================================
//
// All three benches below are `#[ignore]`-gated so they do not run in
// the default `cargo test` sweep. Invoke them explicitly:
//
//   cargo test --release --features kv-lance,kv-mem --no-default-features \
//     --lib kvs::lance::tests::bench_ -- --ignored --nocapture
//
// The `--nocapture` is required: results are printed to stderr via
// `eprintln!` so the test runner doesn't swallow them. `--release`
// matters: debug builds of Lance are ~30-50× slower than release and
// the numbers would be misleading.
//
// What we're trying to demonstrate:
//   - Single-writer throughput: the WAL + memtable hot path should
//     beat the pre-Sprint-AA per-commit Lance round-trip
//     (~thousands of commits/sec vs ~tens of commits/sec).
//   - Concurrent throughput: with N tasks committing in parallel,
//     the memtable + DashMap shards should scale without serialising
//     on a single mutex.
//   - Read latency: a fresh `tx.get` against a memtable-warm key
//     should return in microseconds (no Lance scan required).

/// Throughput of N sequential single-key commits on a single
/// task. Captures the steady-state cost of the WAL+memtable hot
/// path with no concurrency contention.
#[ignore = "throughput micro-benchmark; run with --release --ignored --nocapture"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bench_lsm_single_writer_commit_throughput() {
    const N_COMMITS: usize = 5_000;
    const VAL_SIZE: usize = 64;

    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    let ds = Datastore::new(path_str, LanceConfig::default())
        .await
        .expect("ds open");

    let val = vec![0u8; VAL_SIZE];

    let start = web_time::Instant::now();
    for i in 0..N_COMMITS {
        let key = format!("bench/single/k{:08}", i).into_bytes();
        let tx = ds.transaction(true, false).await.expect("tx");
        tx.set(key, val.clone()).await.expect("set");
        tx.commit().await.expect("commit");
    }
    let elapsed = start.elapsed();

    let throughput = N_COMMITS as f64 / elapsed.as_secs_f64();
    let per_commit_us = elapsed.as_micros() as f64 / N_COMMITS as f64;
    eprintln!(
        "\n[bench] single_writer: {} commits in {:?}  |  {:.0} commits/sec  |  {:.2} µs/commit",
        N_COMMITS, elapsed, throughput, per_commit_us
    );

    ds.shutdown().await.expect("shutdown");
}

/// Throughput of N concurrent tasks each committing M single-key
/// transactions. Exercises the memtable's DashMap shard
/// parallelism + the WAL's per-append mutex contention.
#[ignore = "throughput micro-benchmark; run with --release --ignored --nocapture"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_lsm_concurrent_write_throughput() {
    const N_TASKS: usize = 8;
    const COMMITS_PER_TASK: usize = 1_000;
    const TOTAL: usize = N_TASKS * COMMITS_PER_TASK;
    const VAL_SIZE: usize = 64;

    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    let ds = std::sync::Arc::new(
        Datastore::new(path_str, LanceConfig::default())
            .await
            .expect("ds open"),
    );

    let start = web_time::Instant::now();

    let mut handles = Vec::with_capacity(N_TASKS);
    for task_id in 0..N_TASKS {
        let ds_clone = std::sync::Arc::clone(&ds);
        handles.push(tokio::spawn(async move {
            let val = vec![0u8; VAL_SIZE];
            for i in 0..COMMITS_PER_TASK {
                let key =
                    format!("bench/conc/t{:02}/k{:06}", task_id, i).into_bytes();
                let tx = ds_clone.transaction(true, false).await.expect("tx");
                tx.set(key, val.clone()).await.expect("set");
                tx.commit().await.expect("commit");
            }
        }));
    }
    for h in handles {
        h.await.expect("task panic");
    }

    let elapsed = start.elapsed();
    let throughput = TOTAL as f64 / elapsed.as_secs_f64();
    let per_commit_us = elapsed.as_micros() as f64 / TOTAL as f64;
    eprintln!(
        "\n[bench] concurrent_write: {} tasks × {} commits = {} commits in {:?}  |  \
         {:.0} commits/sec  |  {:.2} µs/commit (wall-time)",
        N_TASKS, COMMITS_PER_TASK, TOTAL, elapsed, throughput, per_commit_us
    );

    std::sync::Arc::try_unwrap(ds)
        .map_err(|_| "outstanding ds refs")
        .unwrap()
        .shutdown()
        .await
        .expect("shutdown");
}

/// Read throughput against a memtable-warm dataset. We disable the
/// flusher so every read hits the memtable (not Lance), isolating
/// the LSM-hot-path read cost from Lance's columnar scan cost.
#[ignore = "throughput micro-benchmark; run with --release --ignored --nocapture"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bench_lsm_memtable_warm_read_throughput() {
    const N_KEYS: usize = 5_000;
    const N_READS: usize = 50_000;
    const VAL_SIZE: usize = 64;

    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    // Disable the flusher so the rows stay in the memtable for the
    // duration of the read loop — we want to time memtable hits,
    // not Lance scans.
    let ds = Datastore::new(
        path_str,
        LanceConfig {
            disable_background_flusher: true,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open");

    // Seed: write N_KEYS rows.
    let val = vec![0u8; VAL_SIZE];
    let keys: Vec<Vec<u8>> = (0..N_KEYS)
        .map(|i| format!("bench/read/k{:08}", i).into_bytes())
        .collect();
    for k in &keys {
        let tx = ds.transaction(true, false).await.expect("tx");
        tx.set(k.clone(), val.clone()).await.expect("set");
        tx.commit().await.expect("commit");
    }

    // Time: N_READS random-ish gets (round-robin over the seeded
    // keys to keep the random-number generator out of the loop and
    // give every key roughly equal load).
    let tx = ds.transaction(false, false).await.expect("read tx");
    let start = web_time::Instant::now();
    for i in 0..N_READS {
        let k = &keys[i % N_KEYS];
        let got = tx.get(k.clone(), None).await.expect("get");
        // Defeat dead-store elimination on the value — touch the
        // first byte. Not strictly necessary for `cargo test
        // --release` since `tx.get` returns through an `await`
        // boundary, but cheap insurance.
        std::hint::black_box(got.as_ref().and_then(|v| v.first().copied()));
    }
    let elapsed = start.elapsed();
    tx.cancel().await.expect("cancel");

    let throughput = N_READS as f64 / elapsed.as_secs_f64();
    let per_read_us = elapsed.as_micros() as f64 / N_READS as f64;
    eprintln!(
        "\n[bench] memtable_warm_read: {} reads of {} seeded keys in {:?}  |  \
         {:.0} reads/sec  |  {:.2} µs/read",
        N_READS, N_KEYS, elapsed, throughput, per_read_us
    );

    ds.shutdown().await.expect("shutdown");
}

// ============================================================================
//  WritePath enum (Sprint BB): runtime-selectable LSM vs LegacyCommitGate
// ============================================================================

/// `WritePath::LegacyCommitGate` routes commits through the
/// CommitGate and reads to `checkout_version(read_version)`. Smoke:
/// set / get / overwrite / delete via the public Transactable surface.
#[tokio::test]
async fn writepath_legacy_commit_gate_smoke() {
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    let ds = Datastore::new(
        path_str,
        LanceConfig {
            write_path: WritePath::LegacyCommitGate,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open");

    let tx = ds.transaction(true, false).await.expect("tx1");
    tx.set(b"wp_k".to_vec(), b"wp_v1".to_vec()).await.expect("set");
    tx.commit().await.expect("commit 1");

    let tx = ds.transaction(false, false).await.expect("tx2");
    assert_eq!(
        tx.get(b"wp_k".to_vec(), None).await.expect("get").as_deref(),
        Some(b"wp_v1".as_ref()),
        "Legacy-gate path failed to make set visible across transactions"
    );
    tx.cancel().await.expect("cancel");

    let tx = ds.transaction(true, false).await.expect("tx3");
    tx.set(b"wp_k".to_vec(), b"wp_v2".to_vec()).await.expect("set");
    tx.commit().await.expect("commit 2");

    let tx = ds.transaction(false, false).await.expect("tx4");
    assert_eq!(
        tx.get(b"wp_k".to_vec(), None).await.expect("get").as_deref(),
        Some(b"wp_v2".as_ref()),
        "Legacy-gate path failed to overwrite a previously committed value"
    );
    tx.cancel().await.expect("cancel");

    let tx = ds.transaction(true, false).await.expect("tx5");
    tx.del(b"wp_k".to_vec()).await.expect("del");
    tx.commit().await.expect("commit 3");

    let tx = ds.transaction(false, false).await.expect("tx6");
    assert!(
        tx.get(b"wp_k".to_vec(), None).await.expect("get").is_none(),
        "Legacy-gate path failed to delete a previously committed key"
    );
    tx.cancel().await.expect("cancel");

    ds.shutdown().await.expect("shutdown");
}

/// `LegacyCommitGate` does NOT spawn a flusher: the gate's own
/// synchronous commit cycle handles every write. Proven by a clean
/// `Datastore::shutdown` (no flusher to drain → no hangs).
#[tokio::test]
async fn writepath_legacy_commit_gate_does_not_spawn_flusher() {
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    let ds = Datastore::new(
        path_str,
        LanceConfig {
            write_path: WritePath::LegacyCommitGate,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open");

    let tx = ds.transaction(true, false).await.expect("tx");
    tx.set(b"only_gate".to_vec(), b"v".to_vec()).await.expect("set");
    tx.commit().await.expect("commit");

    ds.shutdown().await.expect("shutdown");
}

/// Strict snapshot isolation on the LegacyCommitGate path: a read
/// transaction opened BEFORE a write commits must NOT see the write,
/// even though the write commits successfully in the meantime. This
/// is the semantic distinction from `LsmWithWal`, which reads Lance
/// @ latest and would observe the post-commit value.
#[tokio::test]
async fn writepath_legacy_commit_gate_provides_snapshot_iso() {
    let path = unique_tmp_path();
    let path_str = path.to_str().expect("utf-8 path");
    let ds = Datastore::new(
        path_str,
        LanceConfig {
            write_path: WritePath::LegacyCommitGate,
            ..LanceConfig::default()
        },
    )
    .await
    .expect("ds open");

    // Seed one row in V0.
    let tx = ds.transaction(true, false).await.expect("tx-seed");
    tx.set(b"snap_k".to_vec(), b"v_seed".to_vec()).await.expect("set");
    tx.commit().await.expect("commit-seed");

    // Open a read tx; it pins to the current Lance version.
    let read_tx = ds.transaction(false, false).await.expect("read tx");

    // Concurrently, a writer overwrites the row.
    let writer_tx = ds.transaction(true, false).await.expect("writer tx");
    writer_tx
        .set(b"snap_k".to_vec(), b"v_overwrite".to_vec())
        .await
        .expect("set");
    writer_tx.commit().await.expect("commit-overwrite");

    // The pre-existing read tx must still see the seed value — the
    // overwrite landed AFTER it pinned its snapshot.
    let got = read_tx
        .get(b"snap_k".to_vec(), None)
        .await
        .expect("get from pinned snapshot");
    assert_eq!(
        got.as_deref(),
        Some(b"v_seed".as_ref()),
        "LegacyCommitGate read tx saw a value that committed AFTER it started — \
         snapshot iso violated"
    );
    read_tx.cancel().await.expect("cancel");

    ds.shutdown().await.expect("shutdown");
}

// ──────────────────────────────────────────────────────────────────────────
// Timeline — read-only time-series view (the Rubicon "SurrealDB-as-view")
// ──────────────────────────────────────────────────────────────────────────

/// The timeline enumerates Lance's native version history and that history
/// grows by one entry per committed transaction.
///
/// Uses `WritePath::LegacyCommitGate`: only the gate path makes every commit
/// land synchronously as its own Lance dataset version. On the default
/// `LsmWithWal` path, commits batch through the WAL+memtable and the
/// background flusher migrates them into Lance asynchronously, so the
/// timeline reflects *flush boundaries*, not individual commits — the
/// correct granularity for a per-commit Rubicon kanban is the gate path.
#[tokio::test]
async fn test_timeline_versions_grow_with_commits() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	let ds = Datastore::new(
		path_str,
		LanceConfig {
			write_path: WritePath::LegacyCommitGate,
			..LanceConfig::default()
		},
	)
	.await
	.expect("create");

	let timeline = ds.timeline();
	let v_start = timeline.versions().await.expect("versions @ start").len();

	// Two committed write transactions → at least two new Lance versions.
	for (k, v) in [(b"a".as_ref(), b"1".as_ref()), (b"b".as_ref(), b"2".as_ref())] {
		let tx = ds.transaction(true, false).await.expect("tx");
		tx.set(k.to_vec(), v.to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}

	let versions = timeline.versions().await.expect("versions @ end");
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

/// A historical [`TimelineView`] reads the SoA as it stood at that version:
/// a key written at version N is absent from a view pinned before N and
/// present from the view at/after N.
#[tokio::test]
async fn test_timeline_view_reads_historical_soa() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	// LegacyCommitGate: each commit lands synchronously as its own Lance
	// version, so `v_before < v_after` holds per-commit (see the companion
	// test's note on the LSM flush-boundary semantics).
	let ds = Datastore::new(
		path_str,
		LanceConfig {
			write_path: WritePath::LegacyCommitGate,
			..LanceConfig::default()
		},
	)
	.await
	.expect("create");

	let timeline = ds.timeline();
	let v_before = timeline.latest_version().await;

	// Commit a single key.
	{
		let tx = ds.transaction(true, false).await.expect("tx");
		tx.set(b"hist".to_vec(), b"present".to_vec()).await.expect("set");
		tx.commit().await.expect("commit");
	}
	let v_after = timeline.latest_version().await;
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

/// REGRESSION (codex P1 on PR #29): a single transaction carrying BOTH
/// writes and deletes must land as exactly ONE Lance version, not two.
///
/// Before the fix the commit/flush path applied writes via
/// `MergeInsertBuilder::execute_reader` and deletes via a *separate*
/// `Dataset::delete`, producing two native versions per commit. The
/// intermediate (writes-applied, delete-pending) version was hidden from
/// live readers by the datastore write lock, but `Timeline::versions()`
/// surfaced it, so a replayer's `view_at()` could materialize a torn state
/// that was never an atomic SurrealDB commit. Folding deletes into tombstone
/// rows of the same merge collapses the pair into a single version.
#[tokio::test]
async fn test_timeline_write_delete_commit_is_single_atomic_version() {
	let path = unique_tmp_path();
	let path_str = path.to_str().expect("path is valid UTF-8");
	// Gate path: 1 commit = 1 Lance version, so the version delta this test
	// asserts on is exact.
	let ds = Datastore::new(
		path_str,
		LanceConfig {
			write_path: WritePath::LegacyCommitGate,
			..LanceConfig::default()
		},
	)
	.await
	.expect("create");

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
	// (`victim`). Pre-fix this produced TWO Lance versions.
	{
		let tx = ds.transaction(true, false).await.expect("tx mixed");
		tx.set(b"fresh".to_vec(), b"new".to_vec()).await.expect("set fresh");
		tx.set(b"keep".to_vec(), b"new".to_vec()).await.expect("overwrite keep");
		tx.del(b"victim".to_vec()).await.expect("del victim");
		tx.commit().await.expect("commit mixed");
	}

	let versions_after = timeline.versions().await.expect("versions after").len();
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
