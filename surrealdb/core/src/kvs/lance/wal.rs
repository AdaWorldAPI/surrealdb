#![cfg(feature = "kv-lance")]

//! Write-Ahead Log for the kv-lance backend.
//!
//! The WAL provides durability for writes that have landed in the
//! in-memory [`Memtable`](super::memtable) but have not yet been
//! flushed to the underlying Lance dataset. On commit:
//!
//! ```text
//!   writer → WAL.append (fsync) → Memtable.insert → reply Ok
//! ```
//!
//! On restart, the WAL is replayed into the Memtable before the
//! datastore accepts new writes:
//!
//! ```text
//!   open dataset → WAL.replay → Memtable.rebuild → resume
//! ```
//!
//! After a successful background flush of memtable rows N..M into
//! Lance, the flusher calls [`Wal::truncate_to`] to drop records up
//! to (but not including) generation M+1.
//!
//! ## On-disk format
//!
//! Append-only sequence of length-prefixed CBOR records:
//!
//! ```text
//!   [u32 LE record_len][CBOR-encoded WalRecord ... record_len bytes]
//!   [u32 LE record_len][CBOR-encoded WalRecord ... record_len bytes]
//!   ...
//! ```
//!
//! A truncated record at the tail (incomplete length prefix or
//! incomplete body) is silently dropped during replay — this is the
//! crash-recovery contract: if the last `append` did not complete its
//! `fsync`, the commit is rolled back implicitly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::warn;

use crate::kvs::err::{Error, Result};
use crate::kvs::{Key, Val};

/// Tracing target for replay warnings.
const REPLAY_TARGET: &str = "surrealdb::core::kvs::lance::wal::replay";

/// Sanity cap on the size of an individual WAL record body, in bytes.
///
/// Replayed records larger than this are treated as corruption (a
/// scrambled length prefix can decode to a huge number that would
/// otherwise trigger a many-GB allocation). 64 MiB is large enough
/// to absorb realistic batch commits (the build_write_batch_lance
/// pipeline tops out well below this) while small enough that an
/// allocation failure is impossible on any production host.
const MAX_REPLAY_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// A single committed transaction's worth of operations.
///
/// Records carry an opaque `generation` (monotonically increasing per
/// commit on this datastore) so the flusher knows what to truncate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WalRecord {
	pub generation: u64,
	pub ops: Vec<WalOp>,
}

/// A single key-level operation inside a `WalRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum WalOp {
	Set { key: Key, val: Val },
	Delete { key: Key },
}

/// File-backed append-only log.
///
/// Cheap to clone: the inner file handle is wrapped in `Arc<Mutex<…>>`
/// so concurrent appenders serialize on a single per-WAL mutex while
/// still allowing the rest of the datastore to make progress.
pub(super) struct Wal {
	file: Mutex<File>,
	path: PathBuf,
}

impl Wal {
	/// File name used inside the Lance dataset's directory.
	pub(super) const FILE_NAME: &'static str = "kvs_lance.wal";

	/// Open (or create) the WAL file at `<dataset_dir>/kvs_lance.wal`.
	///
	/// The file is opened with `O_RDWR | O_CREAT | O_APPEND`-ish
	/// semantics: appends always land at EOF, but reads can seek to
	/// the beginning for replay/truncate.
	pub(super) async fn open(dataset_dir: &Path) -> Result<Arc<Self>> {
		// Ensure the parent directory exists. Lance creates the dataset
		// dir at `Dataset::write` time, but we open the WAL from
		// `Datastore::new` which may run before that on a fresh path.
		tokio::fs::create_dir_all(dataset_dir).await.map_err(|e| {
			Error::Datastore(format!("wal mkdir {}: {e}", dataset_dir.display()))
		})?;

		let path = dataset_dir.join(Self::FILE_NAME);
		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(&path)
			.await
			.map_err(|e| Error::Datastore(format!("wal open {}: {e}", path.display())))?;

		Ok(Arc::new(Self {
			file: Mutex::new(file),
			path,
		}))
	}

	/// Append one record to the WAL and `fsync` before returning.
	///
	/// The fsync is what gives the writer's `commit()` its durability
	/// guarantee: when this returns `Ok`, the record is on disk and
	/// will be re-applied on restart even if the process crashes
	/// before the background flusher pushes the corresponding memtable
	/// rows into Lance.
	pub(super) async fn append(&self, record: &WalRecord) -> Result<()> {
		let mut bytes = Vec::with_capacity(256);
		ciborium::ser::into_writer(record, &mut bytes)
			.map_err(|e| Error::Datastore(format!("wal serialize: {e}")))?;
		let len = u32::try_from(bytes.len()).map_err(|_| {
			Error::Datastore(format!("wal record too large: {} bytes", bytes.len()))
		})?;

		let mut file = self.file.lock().await;
		// `OpenOptions` above did not set `.append(true)` because we
		// also want to be able to `seek(0)` for replay; instead, seek
		// to the current EOF before each write so concurrent
		// appenders never overwrite each other's payloads.
		file.seek(std::io::SeekFrom::End(0))
			.await
			.map_err(|e| Error::Datastore(format!("wal seek end: {e}")))?;
		file.write_all(&len.to_le_bytes())
			.await
			.map_err(|e| Error::Datastore(format!("wal write len: {e}")))?;
		file.write_all(&bytes)
			.await
			.map_err(|e| Error::Datastore(format!("wal write body: {e}")))?;
		file.sync_all()
			.await
			.map_err(|e| Error::Datastore(format!("wal fsync: {e}")))?;
		Ok(())
	}

	/// Read every committed record from the start of the WAL.
	///
	/// # Recovery contract
	///
	/// Replay never errors out on a corrupted tail. Instead, the
	/// first sign of corruption STOPS the replay and returns the
	/// clean prefix read so far. The four corruption modes recognised:
	///
	/// 1. **Clean EOF** between records — the normal stop condition.
	/// 2. **Truncated length prefix** (less than 4 bytes left) — last
	///    `append` died before writing its prefix. Stop silently.
	/// 3. **Truncated body** (length prefix says N bytes, file has
	///    fewer) — last `append` died before flushing its body. Stop
	///    silently.
	/// 4. **Implausible length prefix** (> `MAX_REPLAY_RECORD_BYTES`)
	///    or **CBOR deserialization failure** on the body — data
	///    corruption (not a clean partial-write). Log WARN with the
	///    file offset, then stop. The clean prefix is returned;
	///    everything past the corruption is dropped.
	///
	/// I/O errors that are NOT EOF-like (e.g. permission denied,
	/// disk read error) still bubble up as `Err` — those signal a
	/// systemic problem the caller needs to know about.
	pub(super) async fn replay(&self) -> Result<Vec<WalRecord>> {
		let mut file = self.file.lock().await;
		file.seek(std::io::SeekFrom::Start(0))
			.await
			.map_err(|e| Error::Datastore(format!("wal seek start: {e}")))?;
		let mut reader = BufReader::new(&mut *file);

		let mut records = Vec::new();
		// `offset` tracks bytes consumed by the reader so warnings
		// can point at the exact byte where corruption was detected.
		let mut offset: u64 = 0;
		loop {
			let record_start = offset;
			let mut len_buf = [0u8; 4];
			match reader.read_exact(&mut len_buf).await {
				Ok(_n) => offset += 4,
				Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
				Err(e) => return Err(Error::Datastore(format!("wal read len: {e}"))),
			}
			let len = u32::from_le_bytes(len_buf) as usize;

			// Corruption mode (4a): an implausibly large length is
			// the classic symptom of a scrambled length prefix
			// (e.g. random bytes appended past a clean tail).
			// Allocating `vec![0; len]` for a multi-GB value would
			// either OOM or succeed and then fail to read enough
			// bytes — both unhelpful. Stop here instead.
			if len > MAX_REPLAY_RECORD_BYTES {
				warn!(
					target: REPLAY_TARGET,
					"wal replay: implausible record length {len} bytes \
                     at offset {record_start} (cap {MAX_REPLAY_RECORD_BYTES}); \
                     treating as corruption — stopping replay, \
                     {} clean records recovered",
					records.len()
				);
				break;
			}

			let mut body = vec![0u8; len];
			match reader.read_exact(&mut body).await {
				Ok(_n) => offset += len as u64,
				// Partial body at EOF — last `append` did not flush
				// its body. Drop the partial record per the
				// recovery contract (mode 3).
				Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
				Err(e) => return Err(Error::Datastore(format!("wal read body: {e}"))),
			}

			// Corruption mode (4b): the body bytes don't decode as a
			// valid `WalRecord`. Could be flipped bits in the body,
			// or a length prefix that scanned past the real record
			// boundary and the bytes happen to start a CBOR object
			// of the right size. Either way, the record is unusable.
			// Stop replay here: we can't trust anything past this
			// offset either (a scrambled length prefix would point
			// us at arbitrary bytes).
			match ciborium::de::from_reader::<WalRecord, _>(&body[..]) {
				Ok(record) => records.push(record),
				Err(e) => {
					warn!(
						target: REPLAY_TARGET,
						"wal replay: deserialize failed at offset {record_start} \
                         ({len} bytes): {e}; treating as corruption — \
                         stopping replay, {} clean records recovered",
						records.len()
					);
					break;
				}
			}
		}
		Ok(records)
	}

	/// Rewrite the WAL file containing only records whose generation
	/// is greater than or equal to `min_generation`.
	///
	/// Called by the flusher after a successful Lance commit: every
	/// record with `generation < min_generation` is now durable in
	/// the Lance dataset and can be removed from the WAL.
	///
	/// Implementation: replay → filter → write a fresh sibling file
	/// → `rename` over the original. The rename is atomic on POSIX,
	/// so a crash mid-truncate leaves either the old WAL or the new
	/// one intact, never a half-truncated mix.
	pub(super) async fn truncate_to(&self, min_generation: u64) -> Result<()> {
		let records = self.replay().await?;
		let retained: Vec<WalRecord> = records
			.into_iter()
			.filter(|r| r.generation >= min_generation)
			.collect();

		let tmp_path = self.path.with_extension("wal.tmp");
		// Build the new file's bytes in memory then write+fsync+rename.
		// WAL sizes are bounded by the configured flush threshold (KB
		// scale), so this is cheap.
		let mut buf: Vec<u8> = Vec::new();
		for r in &retained {
			let mut body = Vec::with_capacity(256);
			ciborium::ser::into_writer(r, &mut body).map_err(|e| {
				Error::Datastore(format!("wal truncate serialize: {e}"))
			})?;
			let len = u32::try_from(body.len()).map_err(|_| {
				Error::Datastore(format!(
					"wal truncate: record too large: {} bytes",
					body.len()
				))
			})?;
			buf.extend_from_slice(&len.to_le_bytes());
			buf.extend_from_slice(&body);
		}

		// Write the tmp file, fsync, then atomic-rename.
		{
			let mut tmp = OpenOptions::new()
				.write(true)
				.create(true)
				.truncate(true)
				.open(&tmp_path)
				.await
				.map_err(|e| {
					Error::Datastore(format!(
						"wal truncate open tmp {}: {e}",
						tmp_path.display()
					))
				})?;
			tmp.write_all(&buf)
				.await
				.map_err(|e| Error::Datastore(format!("wal truncate write: {e}")))?;
			tmp.sync_all()
				.await
				.map_err(|e| Error::Datastore(format!("wal truncate fsync: {e}")))?;
		}

		// Atomic rename. Hold the file lock during this so concurrent
		// appenders don't race against the rename + reopen below.
		let mut file = self.file.lock().await;
		tokio::fs::rename(&tmp_path, &self.path).await.map_err(|e| {
			Error::Datastore(format!(
				"wal truncate rename {} -> {}: {e}",
				tmp_path.display(),
				self.path.display()
			))
		})?;
		// Reopen the file handle to point at the freshly renamed
		// inode (the old handle still points at the now-detached
		// inode that `rename` replaced).
		*file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(false)
			.truncate(false)
			.open(&self.path)
			.await
			.map_err(|e| {
				Error::Datastore(format!(
					"wal truncate reopen {}: {e}",
					self.path.display()
				))
			})?;

		Ok(())
	}
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
	use super::*;

	fn tmp_dir() -> PathBuf {
		let mut p = std::env::temp_dir();
		p.push(format!("kvs-lance-wal-test-{}", uuid::Uuid::new_v4()));
		p
	}

	#[tokio::test]
	async fn append_then_replay_roundtrips() {
		let dir = tmp_dir();
		let wal = Wal::open(&dir).await.expect("open");
		let r1 = WalRecord {
			generation: 1,
			ops: vec![WalOp::Set {
				key: b"k1".to_vec(),
				val: b"v1".to_vec(),
			}],
		};
		let r2 = WalRecord {
			generation: 2,
			ops: vec![
				WalOp::Set {
					key: b"k2".to_vec(),
					val: b"v2".to_vec(),
				},
				WalOp::Delete {
					key: b"k1".to_vec(),
				},
			],
		};
		wal.append(&r1).await.expect("append 1");
		wal.append(&r2).await.expect("append 2");

		let replayed = wal.replay().await.expect("replay");
		assert_eq!(replayed.len(), 2);
		assert_eq!(replayed[0].generation, 1);
		assert_eq!(replayed[1].generation, 2);
		assert!(matches!(replayed[1].ops[1], WalOp::Delete { .. }));
	}

	#[tokio::test]
	async fn truncate_drops_records_below_threshold() {
		let dir = tmp_dir();
		let wal = Wal::open(&dir).await.expect("open");
		for g in 1..=5 {
			wal.append(&WalRecord {
				generation: g,
				ops: vec![WalOp::Set {
					key: format!("k{g}").into_bytes(),
					val: b"v".to_vec(),
				}],
			})
			.await
			.expect("append");
		}
		wal.truncate_to(4).await.expect("truncate");
		let remaining = wal.replay().await.expect("replay");
		assert_eq!(remaining.len(), 2, "only g=4,5 should remain");
		assert_eq!(remaining[0].generation, 4);
		assert_eq!(remaining[1].generation, 5);
	}

	#[tokio::test]
	async fn replay_survives_reopen() {
		let dir = tmp_dir();
		{
			let wal = Wal::open(&dir).await.expect("open");
			wal.append(&WalRecord {
				generation: 1,
				ops: vec![WalOp::Set {
					key: b"k".to_vec(),
					val: b"v".to_vec(),
				}],
			})
			.await
			.expect("append");
		}
		// Reopen — same path, new handle.
		let wal = Wal::open(&dir).await.expect("reopen");
		let replayed = wal.replay().await.expect("replay");
		assert_eq!(replayed.len(), 1);
		assert_eq!(replayed[0].generation, 1);
	}

	#[tokio::test]
	async fn append_after_truncate_continues_correctly() {
		let dir = tmp_dir();
		let wal = Wal::open(&dir).await.expect("open");
		wal.append(&WalRecord {
			generation: 1,
			ops: vec![WalOp::Set {
				key: b"a".to_vec(),
				val: b"1".to_vec(),
			}],
		})
		.await
		.expect("append 1");
		wal.append(&WalRecord {
			generation: 2,
			ops: vec![WalOp::Set {
				key: b"b".to_vec(),
				val: b"2".to_vec(),
			}],
		})
		.await
		.expect("append 2");
		wal.truncate_to(2).await.expect("truncate");
		wal.append(&WalRecord {
			generation: 3,
			ops: vec![WalOp::Set {
				key: b"c".to_vec(),
				val: b"3".to_vec(),
			}],
		})
		.await
		.expect("append 3 after truncate");
		let replayed = wal.replay().await.expect("replay");
		assert_eq!(replayed.len(), 2);
		assert_eq!(replayed[0].generation, 2);
		assert_eq!(replayed[1].generation, 3);
	}

	// ====================================================================
	//  Replay-resilience tests (Sprint hardening: WAL corruption)
	// ====================================================================
	//
	// All three tests follow the same shape:
	//   1. Build a clean WAL with N records via the public API.
	//   2. Drop the Wal so the file handle is released.
	//   3. Use `tokio::fs` directly to append corruption bytes.
	//   4. Reopen and call `replay`. Expect the N clean records back,
	//      with the corruption silently dropped (per the recovery
	//      contract documented in `Wal::replay`).

	/// Append garbage bytes after a valid record. The garbage's first
	/// 4 bytes parse as some length prefix, then the "body" fails to
	/// deserialize as a WalRecord. Replay must keep the clean record
	/// and drop everything past it.
	#[tokio::test]
	async fn replay_stops_at_garbage_body_keeps_clean_prefix() {
		let dir = tmp_dir();
		// Step 1: one clean record.
		{
			let wal = Wal::open(&dir).await.expect("open");
			wal.append(&WalRecord {
				generation: 1,
				ops: vec![WalOp::Set {
					key: b"clean_k".to_vec(),
					val: b"clean_v".to_vec(),
				}],
			})
			.await
			.expect("append clean");
		}
		// Step 3: append [u32 = 16][16 bytes of garbage] — a
		// plausible length prefix followed by bytes that won't
		// deserialize as a WalRecord.
		let path = dir.join(Wal::FILE_NAME);
		{
			let mut f = tokio::fs::OpenOptions::new()
				.append(true)
				.open(&path)
				.await
				.expect("reopen for append");
			let garbage_len: u32 = 16;
			tokio::io::AsyncWriteExt::write_all(&mut f, &garbage_len.to_le_bytes())
				.await
				.expect("write garbage len");
			tokio::io::AsyncWriteExt::write_all(&mut f, &[0xFFu8; 16])
				.await
				.expect("write garbage body");
			tokio::io::AsyncWriteExt::flush(&mut f).await.expect("flush");
		}
		// Step 4: reopen + replay. Clean record survives, garbage dropped.
		let wal = Wal::open(&dir).await.expect("reopen");
		let replayed = wal.replay().await.expect("replay");
		assert_eq!(replayed.len(), 1, "garbage record leaked into replay output");
		assert_eq!(replayed[0].generation, 1);
	}

	/// Append a length prefix that's larger than MAX_REPLAY_RECORD_BYTES
	/// — classic symptom of a scrambled prefix. Replay must stop at
	/// the bad prefix without attempting the huge allocation.
	#[tokio::test]
	async fn replay_stops_at_implausible_length_prefix() {
		let dir = tmp_dir();
		{
			let wal = Wal::open(&dir).await.expect("open");
			wal.append(&WalRecord {
				generation: 7,
				ops: vec![WalOp::Set {
					key: b"k".to_vec(),
					val: b"v".to_vec(),
				}],
			})
			.await
			.expect("append clean");
		}
		let path = dir.join(Wal::FILE_NAME);
		{
			let mut f = tokio::fs::OpenOptions::new()
				.append(true)
				.open(&path)
				.await
				.expect("reopen for append");
			// 1 GiB length prefix — well past the 64 MiB cap.
			let huge_len: u32 = 1_073_741_824;
			tokio::io::AsyncWriteExt::write_all(&mut f, &huge_len.to_le_bytes())
				.await
				.expect("write huge len");
			tokio::io::AsyncWriteExt::flush(&mut f).await.expect("flush");
		}
		let wal = Wal::open(&dir).await.expect("reopen");
		let replayed = wal.replay().await.expect("replay");
		assert_eq!(replayed.len(), 1);
		assert_eq!(replayed[0].generation, 7);
	}

	/// Append a length prefix without a body (less than `len` bytes
	/// after the prefix). This is the "pre-fsync crash" mode and was
	/// already handled by the original replay; the test pins the
	/// behaviour explicitly so the new hardening can't regress it.
	#[tokio::test]
	async fn replay_stops_at_truncated_body_keeps_clean_prefix() {
		let dir = tmp_dir();
		{
			let wal = Wal::open(&dir).await.expect("open");
			wal.append(&WalRecord {
				generation: 42,
				ops: vec![WalOp::Set {
					key: b"survivor".to_vec(),
					val: b"vv".to_vec(),
				}],
			})
			.await
			.expect("append clean");
		}
		let path = dir.join(Wal::FILE_NAME);
		{
			let mut f = tokio::fs::OpenOptions::new()
				.append(true)
				.open(&path)
				.await
				.expect("reopen for append");
			// Claim 100 bytes but only write 4.
			let promised_len: u32 = 100;
			tokio::io::AsyncWriteExt::write_all(&mut f, &promised_len.to_le_bytes())
				.await
				.expect("write len");
			tokio::io::AsyncWriteExt::write_all(&mut f, &[0xAB, 0xCD, 0xEF, 0x01])
				.await
				.expect("write partial body");
			tokio::io::AsyncWriteExt::flush(&mut f).await.expect("flush");
		}
		let wal = Wal::open(&dir).await.expect("reopen");
		let replayed = wal.replay().await.expect("replay");
		assert_eq!(replayed.len(), 1);
		assert_eq!(replayed[0].generation, 42);
	}
}
