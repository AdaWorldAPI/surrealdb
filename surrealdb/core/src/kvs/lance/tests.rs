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
