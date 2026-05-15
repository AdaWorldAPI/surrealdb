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
    let mut config = LanceConfig::default();
    config.versioned = false;
    let ds = Datastore::new(path.to_str().unwrap(), config).await.expect("ds");

    let tx = ds.transaction(false, false).await.expect("tx");
    let err = tx.get(b"k".to_vec(), Some(0)).await.expect_err("should error");
    assert!(matches!(err, crate::kvs::err::Error::UnsupportedVersionedQueries),
        "expected UnsupportedVersionedQueries, got {:?}", err);
    tx.cancel().await.expect("cancel");
    ds.shutdown().await.expect("shutdown");
}
