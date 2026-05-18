//! End-to-end test: SurrealDB live query → ractor mailbox.
//!
//! Spins up an in-memory SurrealDB instance, spawns an `EventCounter` actor,
//! starts a [`LiveQueryRouter`] watching the `events` table, INSERTs three
//! records, and asserts the counter reached 3.
//!
//! See §5 of .claude/plans/integration-plan.md (Sprint 1 acceptance criteria).

use std::time::Duration;

use ractor::{Actor, ActorProcessingErr, ActorRef};
use surrealdb::engine::any;
use surrealdb::opt::capabilities::Capabilities;
use surrealdb::opt::Config;
use surrealdb::types::Value;
use surrealdb_ractor::delta::LiveDelta;
use surrealdb_ractor::router::LiveQueryRouter;

// ── Message type ─────────────────────────────────────────────────────────────

/// Messages sent to [`EventCounter`].
#[derive(Debug)]
enum CounterMsg {
    /// A live-query delta arrived.
    Delta(LiveDelta),
    /// Request the current count via a one-shot reply channel.
    GetCount(ractor::RpcReplyPort<u64>),
}

impl From<LiveDelta> for CounterMsg {
    fn from(d: LiveDelta) -> Self {
        CounterMsg::Delta(d)
    }
}

// ── Actor ────────────────────────────────────────────────────────────────────

/// Counts `LiveDelta::Create` events received via the mailbox.
struct EventCounter;

impl Actor for EventCounter {
    type Msg = CounterMsg;
    type State = u64;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: (),
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(0)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            CounterMsg::Delta(LiveDelta::Create(_)) => {
                *state += 1;
            }
            CounterMsg::Delta(_) => {}
            CounterMsg::GetCount(reply) => {
                let _ = reply.send(*state);
            }
        }
        Ok(())
    }
}

// ── Test ─────────────────────────────────────────────────────────────────────

/// Poll `actor` until its count reaches `expected` or the timeout expires.
async fn wait_for_count(actor: &ActorRef<CounterMsg>, expected: u64, timeout: Duration) -> u64 {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Use ractor's call helper to send GetCount and await the reply.
        let count = ractor::call!(actor, CounterMsg::GetCount).unwrap_or(0);
        if count >= expected {
            return count;
        }
        if tokio::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn live_query_routes_creates_to_actor() {
    // 1. Connect to an in-memory SurrealDB instance.
    //    `Capabilities::all()` enables live-query notifications on the local
    //    engine (gated by `allows_live_query_notifications()` in
    //    `surrealdb/src/engine/local/native.rs:139`).
    let config = Config::default().capabilities(Capabilities::all());
    let db = any::connect(("mem://", config)).await.expect("connect");
    db.use_ns("test").use_db("test").await.expect("use_ns/use_db");

    // 2. Define the table before subscribing.
    //    In 3.1.0-alpha a `LIVE SELECT` on an undefined table returns
    //    "The table 'X' does not exist".
    db.query("DEFINE TABLE events SCHEMALESS")
        .await
        .expect("define table")
        .check()
        .expect("define table result");

    // 3. Spawn the counter actor.
    let (actor, _handle) = Actor::spawn(None, EventCounter, ())
        .await
        .expect("spawn actor");

    // 4. Clone the db handle for the router (it's Arc-backed).
    let db_router = db.clone();

    // 5. Start the router in a background task.
    let actor_clone = actor.clone();
    let router_handle = tokio::spawn(async move {
        LiveQueryRouter {
            db: db_router,
            query: "events".to_string(),
            actor: actor_clone,
        }
        .run()
        .await
        .expect("router exited with error");
    });

    // 6. Give the live query subscription a moment to register.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 7. INSERT three records into the `events` table.
    for i in 0u32..3 {
        let _: Option<Value> = db
            .create("events")
            .content(surrealdb::types::object! {
                "index": i,
                "kind": "test",
            })
            .await
            .expect("create record");
    }

    // 8. Wait up to 5 s for the counter to reach 3.
    let count = wait_for_count(&actor, 3, Duration::from_secs(5)).await;

    // 9. Assert.
    assert_eq!(count, 3, "expected 3 Create events, got {count}");

    // 10. Clean shutdown: stop the actor (router will exit on mailbox close).
    actor.stop(None);
    // Give the router task a chance to exit gracefully.
    let _ = tokio::time::timeout(Duration::from_secs(1), router_handle).await;
}
