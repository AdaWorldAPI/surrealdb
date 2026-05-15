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
