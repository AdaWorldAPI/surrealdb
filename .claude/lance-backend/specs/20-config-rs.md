# AGENT 2 — surrealdb/core/src/kvs/config.rs (LanceConfig only)

Edit ONLY the `#[cfg(feature="kv-lance")]` LanceConfig region. Read it first.

REMOVE:
- `pub enum WritePath { … }` (entire enum + its derives/doc).
- `LanceConfig.write_path` field.
- `LanceConfig.flusher_tick_interval` field.
- `LanceConfig.disable_background_flusher` field (if present).
- any `use`/refs to `WritePath`.
KEEP: `LanceConfig { versioned, retention_ns, … }` and whatever non-flusher
knobs exist. Update `Default for LanceConfig` and `from_params` to drop the
removed fields. Do not touch SurrealKvConfig/RocksDbConfig/MemoryConfig.
Leave a `// ///REVIEW:` on any field you are unsure whether to keep.
