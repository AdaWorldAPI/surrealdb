//! # surrealdb-ractor — Glue #1
//!
//! This crate is the bridge between SurrealDB's live-query / change-feed
//! subsystem and the [`ractor`] actor framework.
//!
//! ## Role (§5 of .claude/plans/integration-plan.md)
//!
//! SurrealDB can emit change-feed events whenever a record is created,
//! updated, or deleted. This crate consumes those events, converts each
//! event into a typed [`delta::LiveDelta`] value (backed by Apache Arrow
//! `RecordBatch`), and routes that delta into a [`ractor`] actor's mailbox
//! via [`router::LiveQueryRouter`].
//!
//! The convenience entry point [`stream::live_stream`] wraps the whole
//! pipeline as a `futures::Stream`.
//!
//! ## Sprint sequence
//!
//! | Sprint | Goal |
//! |--------|------|
//! | 1 (SD-1) | Scaffold — every public item is a stub with `unimplemented!` |
//! | 2 | Wire SurrealDB live-query subscription; emit `LiveDelta` values |
//! | 3 | Route `LiveDelta` into ractor mailboxes; back-pressure handling |
//! | 4 | Integration tests + bench; promote to `0.2.0` |
//!
//! ## Modules

pub mod delta;
pub mod router;
pub mod stream;
