//! Change-event delta types.
//!
//! See §5 of .claude/plans/integration-plan.md for the canonical API sketch.

use std::sync::Arc;

use arrow_array::array::StringArray;
use arrow_array::{RecordBatch, array::Array};
use arrow_schema::{DataType, Field, Schema};

/// A single change-feed event emitted by SurrealDB, expressed as Arrow
/// `RecordBatch` columns so that downstream Lance / DataFusion steps can
/// consume it without copying.
///
/// **Sprint 1 encoding**: each `RecordBatch` contains a single `Utf8` column
/// named `"payload"` whose sole row holds the JSON serialisation of the
/// SurrealDB `Value` that arrived in the notification.  Sprint 2 will refine
/// this to proper typed columns driven by the catalog schema.
///
/// §5 of .claude/plans/integration-plan.md: "each live-query notification
/// is mapped to one `LiveDelta` variant."
#[derive(Debug, Clone)]
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
        ///
        /// Note: In SurrealDB 3.1.0-alpha the live-query notification does not
        /// carry a `before` snapshot; this field mirrors `after` for now.
        /// Sprint 2 can wire the `INCLUDE ORIGINAL` clause when support lands.
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
    /// Arrow schema used for Sprint-1 single-column JSON batches.
    ///
    /// The schema contains one nullable `Utf8` column named `"payload"`.
    pub fn sprint1_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![Field::new("payload", DataType::Utf8, true)]))
    }

    /// Build a single-row Sprint-1 `RecordBatch` from a JSON string.
    ///
    /// Returns an error if Arrow fails to construct the batch (should not
    /// happen with a valid UTF-8 string).
    pub(crate) fn batch_from_json(json: String) -> anyhow::Result<RecordBatch> {
        let schema = Self::sprint1_schema();
        let col: Arc<dyn Array> = Arc::new(StringArray::from(vec![Some(json.as_str())]));
        Ok(RecordBatch::try_new(schema, vec![col])?)
    }

    /// Extract a typed primary-key value from the embedded `RecordBatch`.
    ///
    /// `name` is the Arrow column name that holds the primary key.
    ///
    /// §5 of .claude/plans/integration-plan.md — Sprint 2 refinement needed.
    /// Currently returns `Err` because Sprint-1 batches use a single JSON
    /// `"payload"` column, not typed per-field columns.
    pub fn primary_key<T>(&self, name: &str) -> anyhow::Result<T> {
        let _ = name;
        anyhow::bail!(
            "primary_key() is not yet implemented (Sprint 1 uses opaque JSON payload column); \
             wire typed columns in Sprint 2"
        )
    }

    /// Consume the delta and return the *after* `RecordBatch`.
    ///
    /// For `Create` and `Delete` this is the single batch; for `Update` it
    /// is the `after` snapshot.
    ///
    /// §5 of .claude/plans/integration-plan.md.
    pub fn into_record_batch(self) -> RecordBatch {
        match self {
            LiveDelta::Create(b) | LiveDelta::Delete(b) => b,
            LiveDelta::Update { after, .. } => after,
        }
    }
}
