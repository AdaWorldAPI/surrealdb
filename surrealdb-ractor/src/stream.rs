//! Convenience streaming entry-point.
//!
//! See §5 of .claude/plans/integration-plan.md for the canonical API sketch.

use futures::Stream;

use crate::delta::LiveDelta;

/// Subscribe to a SurrealDB live query and return the change-feed as an
/// async `Stream` of typed [`LiveDelta`] values.
///
/// Each item in the stream corresponds to one create / update / delete event
/// emitted by SurrealDB for the given `query` statement. Errors from the
/// underlying subscription surface as `Err(anyhow::Error)` items rather than
/// terminating the stream immediately, letting callers decide on retry policy.
///
/// ## Parameters
///
/// * `query` — a SurrealQL `LIVE SELECT …` statement.
///
/// ## Example (Sprint 2+)
///
/// ```rust,ignore
/// use futures::StreamExt;
/// use surrealdb_ractor::stream::live_stream;
///
/// let mut stream = live_stream("LIVE SELECT * FROM orders").await?;
/// while let Some(delta) = stream.next().await {
///     println!("{delta:?}");
/// }
/// ```
///
/// §5 of .claude/plans/integration-plan.md — stub; implemented in Sprint 2.
pub async fn live_stream(
    query: &str,
) -> anyhow::Result<impl Stream<Item = anyhow::Result<LiveDelta>>> {
    let _ = query;
    unimplemented!("SD-1 stub — Sprint 1")
}
