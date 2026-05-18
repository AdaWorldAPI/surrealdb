//! Pluggable MVCC version-number source for SurrealDB storage backends.
//!
//! # Why this exists
//!
//! Different storage backends require different strategies for generating
//! monotonically-increasing version numbers used to implement MVCC
//! (multi-version concurrency control):
//!
//! - **Local backends** (mem, RocksDB, SurrealKV, Lance): a process-local
//!   [`AtomicU64`] counter is sufficient because there is only one writer
//!   process.  [`LocalGeneratedMvcc`] provides this default.
//!
//! - **Distributed backends** (kv-tikv with native-MVCC): the PD (Placement
//!   Driver) cluster issues HLC (Hybrid Logical Clock) timestamps that are
//!   globally ordered.  Sprint 2 will add `impl MvccSource for
//!   TikvNativeMvccTxn` that delegates to the TiKV client — that
//!   implementation is **out of scope here** and will be added
//!   in `kvs/tikv/native_mvcc.rs` without touching this file.
//!
//! Making the source pluggable through a trait keeps the transaction layer
//! free from backend-specific timestamp logic and makes it straightforward
//! to compose alternative clock implementations in tests.
//!
//! # Additive shape
//!
//! This module introduces a **new** public trait and a single concrete
//! impl.  No existing trait or struct is modified.  The orchestrator
//! adds the `pub mod mvcc_source;` declaration to `kvs/mod.rs` during
//! consolidation; individual backends opt in by implementing the trait.
//!
//! # References
//!
//! See `.claude/plans/integration-plan.md §1` (Contracts table —
//! `MvccSource` trait sketch) and `§5b` (kv-tikv-native-mvcc consumer).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

/// A source of monotonically non-decreasing MVCC version numbers.
///
/// Implementors are expected to be cheaply cloneable (e.g. `Arc`-wrapped)
/// and safe to share across async tasks.
///
/// # Contract
///
/// - Each call to [`next_version`] MUST return a value `≥` any value
///   previously returned by the same instance.
/// - Values need not be consecutive; gaps are permitted (for example, an
///   HLC-backed source will return wall-clock milliseconds which can jump).
/// - Implementations must be `Send + Sync` so they can be stored inside
///   `Arc<dyn MvccSource>` and shared across Tokio tasks.
///
/// [`next_version`]: MvccSource::next_version
pub trait MvccSource: Send + Sync {
	/// Return the next MVCC version number.
	///
	/// The returned `Future` must be `Send` so that callers can `.await` it
	/// inside Tokio multi-threaded runtimes.
	fn next_version(&self) -> impl Future<Output = anyhow::Result<u64>> + Send;
}

// ---------------------------------------------------------------------------
// Default implementation — process-local atomic counter
// ---------------------------------------------------------------------------

/// A process-local MVCC version source backed by an [`AtomicU64`] counter.
///
/// This matches SurrealDB's current behaviour for single-node backends: a
/// simple fetch-and-increment that returns a strictly increasing `u64` on
/// every call.  It is **not** suitable for distributed deployments where
/// multiple writer processes must agree on a global ordering.
///
/// # Example
///
/// ```rust,ignore
/// use crate::kvs::mvcc_source::{LocalGeneratedMvcc, MvccSource};
///
/// let src = LocalGeneratedMvcc::new();
/// let v1 = src.next_version().await.unwrap();
/// let v2 = src.next_version().await.unwrap();
/// assert!(v2 > v1);
/// ```
pub struct LocalGeneratedMvcc {
	/// The underlying monotonic counter.  Starts at 1 so that version 0 can
	/// be used as a sentinel "no version" value by callers if needed.
	counter: AtomicU64,
}

impl LocalGeneratedMvcc {
	/// Create a new counter starting at version `1`.
	pub fn new() -> Self {
		Self {
			counter: AtomicU64::new(1),
		}
	}
}

impl Default for LocalGeneratedMvcc {
	fn default() -> Self {
		Self::new()
	}
}

impl MvccSource for LocalGeneratedMvcc {
	/// Atomically increment the counter and return the previous value.
	///
	/// This is an infallible, synchronous operation wrapped in an immediately-
	/// ready `Future` so it conforms to the async trait contract.  The
	/// [`Ordering::Relaxed`] store is sufficient because the counter is
	/// always accessed through a single `AtomicU64` with sequentially-
	/// consistent fetch-add semantics on the same process.
	fn next_version(&self) -> impl Future<Output = anyhow::Result<u64>> + Send {
		let v = self.counter.fetch_add(1, Ordering::SeqCst);
		std::future::ready(Ok(v))
	}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::{LocalGeneratedMvcc, MvccSource};

	/// Verify that successive calls to `next_version` are strictly increasing.
	#[tokio::test]
	async fn local_mvcc_is_monotonically_increasing() {
		let src = LocalGeneratedMvcc::new();

		let mut prev = src.next_version().await.expect("next_version must not fail");
		for _ in 0..1_000 {
			let next = src.next_version().await.expect("next_version must not fail");
			assert!(
				next > prev,
				"expected version {next} > {prev} but it was not strictly greater"
			);
			prev = next;
		}
	}

	/// Verify that independent instances do not share state.
	#[tokio::test]
	async fn local_mvcc_instances_are_independent() {
		let a = LocalGeneratedMvcc::new();
		let b = LocalGeneratedMvcc::new();

		let va = a.next_version().await.unwrap();
		let vb = b.next_version().await.unwrap();

		// Both start at 1; they must be equal, not one ahead of the other.
		assert_eq!(va, vb, "independent instances must start from the same seed");
	}

	/// Smoke-test the `Default` impl.
	#[test]
	fn default_starts_at_one() {
		let src = LocalGeneratedMvcc::default();
		// fetch_add returns the *old* value, so the first call returns 1.
		let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
		let v = rt.block_on(src.next_version()).unwrap();
		assert_eq!(v, 1, "first version from a fresh counter must be 1");
	}
}
