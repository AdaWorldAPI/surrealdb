#![cfg(test)]
#![cfg(feature = "kv-lance")]

//! Higher-level integration smoke tests for the Lance backend.
//!
//! These tests exercise the full SurrealDB stack — parser + planner +
//! execution engine + Transactable trait — against the kv-lance storage
//! backend, verifying that SurrealQL queries round-trip correctly through
//! the backend.
//!
//! The Datastore is built via `Datastore::builder().build_with_path("lance://...")`
//! which exercises the URL-routing patch in `kvs/ds.rs` (Sprint A4) and the
//! full SurrealDB execution engine, unlike the unit tests in `tests.rs` which
//! call the `Transactable` trait methods directly.
//!
//! # Note on namespace / database setup
//!
//! SurrealDB requires the namespace and database to exist before executing
//! DML. We issue `DEFINE NS` / `DEFINE DB` as a preamble (same as
//! `tests/helpers.rs::new_ns_db`).

use std::env;

use uuid::Uuid;

use crate::dbs::Session;
use crate::kvs::Datastore;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a fresh Datastore on a unique tempdir path via the full builder path.
async fn lance_ds() -> Datastore {
    let tmp = env::temp_dir().join(format!("surreal_lance_smoke_{}", Uuid::new_v4()));
    let url = format!("lance://{}", tmp.display());
    Datastore::builder()
        .build_with_path(&url)
        .await
        .expect("build_with_path(lance://...) should succeed")
}

/// Set up a namespace + database so that DML queries don't fail with
/// "namespace/database not found" errors. Mirrors `helpers.rs::new_ns_db`.
async fn setup_ns_db(ds: &Datastore) {
    let sess = Session::owner().with_ns("test");
    ds.execute("DEFINE NS test", &Session::owner(), None)
        .await
        .expect("DEFINE NS");
    ds.execute("DEFINE DB test", &sess, None)
        .await
        .expect("DEFINE DB");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// CREATE a record and SELECT it back.
///
/// This is the most basic smoke test — it proves the URL routing (lance://),
/// the parser, the planner, and the write + read paths all work together.
#[tokio::test]
async fn smoke_create_select() {
    let ds = lance_ds().await;
    setup_ns_db(&ds).await;
    let session = Session::owner().with_ns("test").with_db("test");

    // CREATE a record
    let res = ds
        .execute("CREATE person:1 SET name = 'Alice', age = 30;", &session, None)
        .await
        .expect("CREATE should not return an Err at the execute level");
    assert_eq!(res.len(), 1, "CREATE should return exactly one query result");
    let create_val = res.into_iter().next().unwrap().result;
    assert!(
        create_val.is_ok(),
        "CREATE result should be Ok, got: {:?}",
        create_val
    );

    // SELECT it back
    let res = ds
        .execute("SELECT * FROM person:1;", &session, None)
        .await
        .expect("SELECT should not return an Err at the execute level");
    assert_eq!(res.len(), 1, "SELECT should return exactly one query result");
    let value = res.into_iter().next().unwrap().result.expect("SELECT result should be Ok");
    // Use Debug formatting for assertion stability across SurrealDB versions —
    // structural pattern-matching on Value is brittle.
    let s = format!("{:?}", value);
    assert!(
        s.contains("Alice"),
        "SELECT result should contain 'Alice', got: {}",
        s
    );
}

/// CREATE then UPDATE then SELECT — verifies the overwrite path works through
/// the full stack, including Sprint F's delete-before-append in commit().
#[tokio::test]
async fn smoke_update_overwrite() {
    let ds = lance_ds().await;
    setup_ns_db(&ds).await;
    let session = Session::owner().with_ns("test").with_db("test");

    // INSERT initial value
    ds.execute("CREATE counter:c SET n = 1;", &session, None)
        .await
        .expect("CREATE")
        .into_iter()
        .next()
        .unwrap()
        .result
        .expect("CREATE result should be Ok");

    // UPDATE to a new value
    ds.execute("UPDATE counter:c SET n = 2;", &session, None)
        .await
        .expect("UPDATE")
        .into_iter()
        .next()
        .unwrap()
        .result
        .expect("UPDATE result should be Ok");

    // Read back — must see the updated value, not the stale one
    let res = ds
        .execute("SELECT n FROM counter:c;", &session, None)
        .await
        .expect("SELECT after UPDATE");
    let value = res.into_iter().next().unwrap().result.expect("SELECT result should be Ok");
    let s = format!("{:?}", value);
    assert!(
        s.contains('2'),
        "UPDATE should overwrite to n=2; got: {}",
        s
    );
    // The old value must NOT appear as a stale row.
    assert!(
        !s.contains("\"1\"") && !s.contains("n: 1"),
        "Stale value n=1 must not appear after UPDATE; got: {}",
        s
    );
}

/// DELETE a record and verify it is gone.
///
/// After `DELETE thing:t`, a subsequent `SELECT * FROM thing:t` must return
/// an empty array — not the deleted record.
#[tokio::test]
async fn smoke_delete() {
    let ds = lance_ds().await;
    setup_ns_db(&ds).await;
    let session = Session::owner().with_ns("test").with_db("test");

    ds.execute("CREATE thing:t SET kind = 'mug';", &session, None)
        .await
        .expect("CREATE")
        .into_iter()
        .next()
        .unwrap()
        .result
        .expect("CREATE result should be Ok");

    ds.execute("DELETE thing:t;", &session, None)
        .await
        .expect("DELETE")
        .into_iter()
        .next()
        .unwrap()
        .result
        .expect("DELETE result should be Ok");

    let res = ds
        .execute("SELECT * FROM thing:t;", &session, None)
        .await
        .expect("SELECT after DELETE");
    let value = res.into_iter().next().unwrap().result.expect("SELECT result should be Ok");
    let s = format!("{:?}", value);
    // After delete, SELECT must return an empty array.
    assert!(
        s == "[]" || s == "Array([])" || s.contains("[]"),
        "SELECT after DELETE should be empty, got: {}",
        s
    );
}
