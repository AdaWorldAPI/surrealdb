//! Convenience streaming entry-point.
//!
//! See §5 of .claude/plans/integration-plan.md for the canonical API sketch.
//!
//! # SurrealDB SDK path used (3.1.0-alpha)
//!
//! ```text
//! surrealdb/src/method/query.rs:281  — QueryStream<R> type
//! surrealdb/src/method/query.rs:164  — live-query UUID extraction
//! surrealdb/src/method/query.rs:394  — IndexedResults::stream()
//! surrealdb/src/method/live.rs:26    — into_future for Select<..,Live>
//! ```
//!
//! Two routes exist for live queries:
//!
//! 1. `db.select("table").live().await?` — yields `Notification<R>` where `R`
//!    is inferred.  A bare `&str` resource only satisfies
//!    `IntoResource<Vec<R>>`, so the stream item type is `Notification<R>`,
//!    not `Notification<Vec<R>>`, but the type annotation must be `Vec<R>`.
//!
//! 2. `db.query("LIVE SELECT * FROM table").await?` followed by
//!    `.stream::<Value>(0)?` — yields `Notification<Value>` and accepts any
//!    SurrealQL string.  **This module uses route #2** because it handles
//!    an arbitrary query string cleanly.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use futures::StreamExt as _;
use surrealdb::Notification;
use surrealdb::engine::any::Any;
use surrealdb::types::{Action, Value};
use surrealdb::{Result as SurrealResult, Surreal};

use crate::delta::LiveDelta;

/// Opaque live-query stream returned by [`live_stream`].
///
/// Implements [`futures::Stream`]`<Item = anyhow::Result<LiveDelta>>`.
/// Each item corresponds to one SurrealDB change-feed notification mapped to
/// the Sprint-1 single-column Arrow `RecordBatch` encoding.
///
/// The stream terminates when the underlying SurrealDB connection closes or
/// when the live query is killed.
///
/// §5 of .claude/plans/integration-plan.md — implemented in Sprint 1.
pub struct LiveStream {
    inner: Pin<Box<dyn Stream<Item = SurrealResult<Notification<Value>>> + Send>>,
}

impl Stream for LiveStream {
    type Item = anyhow::Result<LiveDelta>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(notification))) => {
                Poll::Ready(Some(notification_to_delta(notification)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(anyhow::Error::from(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Convert a single `Notification<Value>` into a [`LiveDelta`].
///
/// Sprint-1 encoding: the SurrealDB `Value` is serialised to JSON and stored
/// as the sole row of a single `Utf8` column named `"payload"`.  Sprint 2 will
/// replace this with typed Arrow columns driven by the catalog schema.
fn notification_to_delta(n: Notification<Value>) -> anyhow::Result<LiveDelta> {
    let json = serde_json::to_string(&n.data)?;
    let batch = LiveDelta::batch_from_json(json)?;
    let delta = match n.action {
        Action::Create => LiveDelta::Create(batch.clone()),
        Action::Update => LiveDelta::Update {
            before: batch.clone(),
            after: batch,
        },
        Action::Delete => LiveDelta::Delete(batch),
        // Killed signals end-of-stream; the outer poll loop handles None.
        Action::Killed => {
            anyhow::bail!("live query killed by server")
        }
    };
    Ok(delta)
}

/// Subscribe to a SurrealDB table's live-query feed and return a typed
/// async `Stream` of [`LiveDelta`] values.
///
/// ## Parameters
///
/// * `db`    — an active, ns/db-selected [`Surreal<Any>`] connection.
/// * `table` — the table name to watch, e.g. `"events"`.  Internally this is
///   wrapped in a `LIVE SELECT * FROM <table>` statement.
///
/// ## SDK path (3.1.0-alpha)
///
/// Uses `db.query("LIVE SELECT * FROM {table}").await?` followed by
/// `response.stream::<Value>(0)?` (
/// `surrealdb/src/method/query.rs:394`).  This path accepts an arbitrary
/// query string and avoids the `IntoResource` trait-bound mismatch that
/// affects `db.select(table).live()` when `table` is a `&str`.
///
/// ## Example
///
/// ```rust,ignore
/// use futures::StreamExt;
/// use surrealdb::engine::any;
/// use surrealdb_ractor::stream::live_stream;
///
/// let db = any::connect("mem://").await?;
/// db.use_ns("test").use_db("test").await?;
/// let mut stream = live_stream(&db, "orders").await?;
/// while let Some(delta) = stream.next().await {
///     println!("{delta:?}");
/// }
/// ```
///
/// §5 of .claude/plans/integration-plan.md — implemented in Sprint 1.
pub async fn live_stream(db: &Surreal<Any>, table: &str) -> anyhow::Result<LiveStream> {
    // Issue the LIVE SELECT via the query path so that any table name string
    // is accepted without IntoResource<Vec<R>> vs IntoResource<Value> issues.
    //
    // SDK path: surrealdb/src/method/query.rs:394 (IndexedResults::stream)
    //           surrealdb/src/method/query.rs:164  (live-query UUID extraction)
    let sql = format!("LIVE SELECT * FROM {table}");
    let mut response = db.query(sql).await?;
    // `.stream::<Value>(0)` returns a `QueryStream<Value>` which is
    // `Stream<Item = Result<Notification<Value>>>`.
    let raw = response.stream::<Value>(0)?;
    Ok(LiveStream {
        inner: Box::pin(raw),
    })
}
