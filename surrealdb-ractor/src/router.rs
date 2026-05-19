//! Live-query router: SurrealDB change-feed events → ractor actor mailboxes.
//!
//! See §5 of .claude/plans/integration-plan.md for the canonical API sketch.

use futures::StreamExt as _;
use ractor::ActorRef;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::delta::LiveDelta;
use crate::stream::live_stream;

/// Routes live-query change-feed events from a SurrealDB connection to a
/// [`ractor`] actor.
///
/// ## Fields
///
/// * `db` — an active, ns/db-selected SurrealDB client/connection handle.
/// * `query` — the table name (or `LIVE SELECT …` SurrealQL statement) that
///   defines which records to watch. For Sprint 1 this is treated as a plain
///   table name passed directly to [`live_stream`].
/// * `actor` — the [`ractor::ActorRef`] whose mailbox receives each
///   [`crate::delta::LiveDelta`] produced by the subscription.
///
/// §5 of .claude/plans/integration-plan.md.
pub struct LiveQueryRouter<M>
where
    M: ractor::Message,
{
    /// The SurrealDB connection/session.
    ///
    /// Must already have `use_ns` / `use_db` called before `run` is invoked.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    pub db: Surreal<Any>,

    /// The table name (or `LIVE SELECT …` SurrealQL query string).
    ///
    /// Sprint 1: treated as a bare table name. Sprint 2 will support full
    /// SurrealQL via the `db.query(…).stream(…)` path.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    pub query: String,

    /// The ractor actor that will receive each `LiveDelta` message.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    pub actor: ActorRef<M>,
}

impl<M> LiveQueryRouter<M>
where
    M: ractor::Message + From<LiveDelta>,
{
    /// Subscribe to the live query and forward each change-feed event to the
    /// actor's mailbox.
    ///
    /// The future drives the subscription loop until either the SurrealDB
    /// connection closes, the live query is killed, or the actor's mailbox
    /// is dropped (send error).
    ///
    /// ## Back-pressure
    ///
    /// [`ractor`] mailboxes are unbounded by default. The `send_message` call
    /// is synchronous (channel send), so this loop never yields to the executor
    /// unnecessarily; it only awaits the next notification from SurrealDB.
    ///
    /// ## Error handling
    ///
    /// * A stream item that is `Err(e)` is logged at `warn` level and skipped;
    ///   the loop continues to drain the stream.
    /// * If the actor mailbox is closed (`send_message` returns `Err`), the
    ///   loop exits and this function returns `Ok(())`.
    ///
    /// §5 of .claude/plans/integration-plan.md — implemented in Sprint 1.
    pub async fn run(self) -> anyhow::Result<()> {
        let mut stream = live_stream(&self.db, &self.query).await?;
        while let Some(item) = stream.next().await {
            match item {
                Ok(delta) => {
                    if self.actor.send_message(M::from(delta)).is_err() {
                        // Mailbox closed — actor stopped; exit cleanly.
                        break;
                    }
                }
                Err(e) => {
                    // Surface non-fatal stream errors but keep running.
                    tracing::warn!(error = %e, "live query stream error; continuing");
                }
            }
        }
        Ok(())
    }
}
