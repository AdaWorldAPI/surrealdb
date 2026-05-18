//! # `CfStream` — Glue \#1 Bridge: Change-Feed Cursor → Actor Inbox
//!
//! ## Why this exists
//!
//! `surrealdb-core` already has a batch-oriented change-feed reader
//! ([`crate::cf::read`]).  The `surrealdb-ractor` actor layer needs to
//! *poll* that feed incrementally — one logical delta at a time — so it can
//! forward deltas to Lance dataset writers without accumulating unbounded
//! batches in memory.
//!
//! `CfStream` is that bridge.  It is an async, single-item-at-a-time
//! abstraction over whatever cursor implementation lives below (the
//! `Vec<ChangeSet>` returned by the existing reader, a future real-time
//! streaming cursor, etc.).
//!
//! ## Position in the additive contract shape
//!
//! This file is purely **additive**: it introduces a new public trait
//! without modifying any existing file.  The `cf/mod.rs` re-export line
//! (`pub mod stream;`) is added by the orchestrator in a separate
//! consolidation commit so that each worker's diff stays orthogonal.
//!
//! ## What this does NOT do
//!
//! * Does **not** modify or wrap [`crate::cf::read`] — that API is
//!   unchanged.
//! * Does **not** introduce a dependency on `surrealdb-ractor`, Arrow, or
//!   any other external crate.  The payload type (`ChangeSet`) is already
//!   defined in this crate, so no new dependency is needed.
//! * Does **not** implement persistence, back-pressure, or reconnection
//!   logic.  Those concerns belong to the actor layer.
//!
//! ## Payload type choice
//!
//! Rather than defining a local `LiveDelta` mirror (which would drift from
//! the canonical type in `surrealdb-ractor`) or pulling in `arrow2` /
//! `RecordBatch` (which would add a heavyweight dep to `surrealdb-core`),
//! this trait surfaces the existing [`crate::cf::ChangeSet`] directly.
//!
//! `ChangeSet` carries `(versionstamp: u128, DatabaseMutation)` — exactly
//! the information the actor needs.  The actor crate converts a `ChangeSet`
//! into its own `LiveDelta` representation; that conversion is a thin map
//! and keeps the dependency arrow pointing in the right direction
//! (actor → core, never core → actor).
//!
//! ## See also
//!
//! `.claude/plans/integration-plan.md §1` (Contracts table — `CfStream`
//! sketch) and `§5` (actor-to-Lance write path that consumes this stream).

use std::future::Future;

use anyhow::Result;

use crate::cf::ChangeSet;

// ---------------------------------------------------------------------------
// Public trait
// ---------------------------------------------------------------------------

/// An async, single-item-at-a-time stream over a SurrealDB change-feed.
///
/// Implementors wrap the batch-oriented [`crate::cf::read`] cursor (or any
/// future streaming cursor) and expose individual [`ChangeSet`] values one
/// at a time so that actor-layer consumers can apply back-pressure.
///
/// # Contract
///
/// * `next_delta` **must not** block the calling thread.  Implementations
///   that perform I/O must do so via async primitives.
/// * Returning `None` signals that the underlying cursor is exhausted for
///   the current window.  Callers are responsible for deciding whether to
///   re-open the stream at the next versionstamp.
/// * Returning `Some(Err(_))` signals a transient or permanent error.
///   Callers should inspect the error and decide on retry policy.
///
/// # WASM note
///
/// Because `surrealdb-core` must compile for WASM targets, the returned
/// `Future` is intentionally **not** required to be `Send`.  Wrap with
/// `async_trait(?Send)` when implementing on types that are not `Send`.
pub trait CfStream {
    /// Yield the next [`ChangeSet`] from the underlying change-feed cursor,
    /// or `None` if the cursor is exhausted.
    ///
    /// On error the stream may or may not be able to continue; check the
    /// error kind and call `next_delta` again only if appropriate.
    fn next_delta(&mut self) -> impl Future<Output = Option<Result<ChangeSet>>>;
}

// ---------------------------------------------------------------------------
// Convenience blanket: Vec<ChangeSet> implements CfStream
// ---------------------------------------------------------------------------
//
// A `Vec<ChangeSet>` drained from front to back is the simplest possible
// implementation.  It lets callers unit-test code that consumes a `CfStream`
// without needing a real datastore.

impl CfStream for std::collections::VecDeque<ChangeSet> {
    async fn next_delta(&mut self) -> Option<Result<ChangeSet>> {
        self.pop_front().map(Ok)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cf::{DatabaseMutation, ChangeSet};
    use std::collections::VecDeque;

    // ---------------------------------------------------------------------------
    // A minimal stub implementation of CfStream used only in tests.
    // ---------------------------------------------------------------------------

    /// Stub that yields a fixed list of [`ChangeSet`] values in order,
    /// then returns `None`.
    struct StubStream {
        items: VecDeque<ChangeSet>,
    }

    impl StubStream {
        fn new(items: Vec<ChangeSet>) -> Self {
            Self {
                items: items.into(),
            }
        }
    }

    impl CfStream for StubStream {
        async fn next_delta(&mut self) -> Option<Result<ChangeSet>> {
            self.items.pop_front().map(Ok)
        }
    }

    // ---------------------------------------------------------------------------
    // Helper: build a minimal ChangeSet with a given versionstamp.
    // ---------------------------------------------------------------------------

    fn make_changeset(version: u128) -> ChangeSet {
        ChangeSet(version, DatabaseMutation::new())
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    /// Draining a stub stream yields items in insertion order, then None.
    #[tokio::test]
    async fn stub_stream_yields_in_order() {
        let mut stream = StubStream::new(vec![
            make_changeset(1),
            make_changeset(2),
            make_changeset(3),
        ]);

        let a = stream.next_delta().await.expect("first item").expect("no error");
        assert_eq!(a.0, 1, "first versionstamp should be 1");

        let b = stream.next_delta().await.expect("second item").expect("no error");
        assert_eq!(b.0, 2, "second versionstamp should be 2");

        let c = stream.next_delta().await.expect("third item").expect("no error");
        assert_eq!(c.0, 3, "third versionstamp should be 3");

        let done = stream.next_delta().await;
        assert!(done.is_none(), "stream should be exhausted after 3 items");
    }

    /// The blanket `VecDeque<ChangeSet>` implementation works the same way.
    #[tokio::test]
    async fn vecdeque_blanket_impl_works() {
        let mut stream: VecDeque<ChangeSet> =
            vec![make_changeset(10), make_changeset(20)].into();

        let first = stream.next_delta().await.expect("item").expect("ok");
        assert_eq!(first.0, 10);

        let second = stream.next_delta().await.expect("item").expect("ok");
        assert_eq!(second.0, 20);

        assert!(stream.next_delta().await.is_none());
    }

    /// A CfStream consumer that counts deltas — exercises the trait as a
    /// generic bound so the compiler verifies it is object-usable.
    async fn count_deltas<S: CfStream>(stream: &mut S) -> usize {
        let mut n = 0usize;
        while let Some(result) = stream.next_delta().await {
            result.expect("no error during count");
            n += 1;
        }
        n
    }

    #[tokio::test]
    async fn generic_consumer_counts_correctly() {
        let mut stream = StubStream::new(vec![
            make_changeset(100),
            make_changeset(200),
            make_changeset(300),
            make_changeset(400),
        ]);
        let count = count_deltas(&mut stream).await;
        assert_eq!(count, 4);
    }
}
