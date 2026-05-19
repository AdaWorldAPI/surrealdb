//! Live-query router: SurrealDB change-feed events → ractor actor mailboxes.
//!
//! See §5 of .claude/plans/integration-plan.md for the canonical API sketch.

use ractor::ActorRef;

/// Routes live-query change-feed events from a SurrealDB connection to a
/// [`ractor`] actor.
///
/// ## Fields
///
/// * `db` — an active SurrealDB client/connection handle.
/// * `query` — the SurrealQL `LIVE SELECT …` statement text that defines
///   which records to watch.
/// * `actor` — the [`ractor::ActorRef`] whose mailbox receives each
///   [`crate::delta::LiveDelta`] produced by the subscription.
///
/// §5 of .claude/plans/integration-plan.md — stub; wired in Sprint 2.
pub struct LiveQueryRouter<M>
where
    M: ractor::Message,
{
    /// The SurrealDB connection/session.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    pub db: surrealdb::Surreal<surrealdb::engine::any::Any>,

    /// The `LIVE SELECT` SurrealQL query string.
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
    M: ractor::Message,
{
    /// Subscribe to the live query and forward each change-feed event to the
    /// actor's mailbox.
    ///
    /// The future drives the subscription loop until either the SurrealDB
    /// connection closes or the actor's mailbox is dropped.
    ///
    /// §5 of .claude/plans/integration-plan.md — stub; implemented in Sprint 2.
    pub async fn run(self) -> anyhow::Result<()> {
        unimplemented!("SD-1 stub — Sprint 1")
    }
}
