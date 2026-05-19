//! Change-event delta types.
//!
//! See §5 of .claude/plans/integration-plan.md for the canonical API sketch.

use arrow_array::RecordBatch;

/// A single change-feed event emitted by SurrealDB, expressed as Arrow
/// `RecordBatch` columns so that downstream Lance / DataFusion steps can
/// consume it without copying.
///
/// §5 of .claude/plans/integration-plan.md: "each live-query notification
/// is mapped to one `LiveDelta` variant."
#[derive(Debug)]
pub enum LiveDelta {
    /// A new record was inserted.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    Create(RecordBatch),

    /// An existing record was mutated.
    ///
    /// Both the `before` and `after` snapshots are carried so that a
    /// downstream actor can compute a diff without re-fetching.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    Update {
        /// The record state immediately before the mutation.
        before: RecordBatch,
        /// The record state immediately after the mutation.
        after: RecordBatch,
    },

    /// A record was removed.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    Delete(RecordBatch),
}

impl LiveDelta {
    /// Extract a typed primary-key value from the embedded `RecordBatch`.
    ///
    /// `name` is the Arrow column name that holds the primary key.
    ///
    /// §5 of .claude/plans/integration-plan.md — stub; implemented in Sprint 2.
    pub fn primary_key<T>(&self, name: &str) -> anyhow::Result<T> {
        let _ = name;
        unimplemented!("SD-1 stub — Sprint 1")
    }

    /// Consume the delta and return the *after* `RecordBatch`.
    ///
    /// For `Create` and `Delete` this is the single batch; for `Update` it
    /// is the `after` snapshot.
    ///
    /// §5 of .claude/plans/integration-plan.md — stub; implemented in Sprint 2.
    pub fn into_record_batch(self) -> RecordBatch {
        unimplemented!("SD-1 stub — Sprint 1")
    }
}
