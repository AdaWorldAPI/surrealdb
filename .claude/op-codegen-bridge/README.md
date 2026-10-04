# op-codegen-bridge

**Initiative:** DDL-friendly constructors and chainable setters for
`crate::catalog::TableDefinition` / `FieldDefinition` / `IndexDefinition`
so external codegen tools can build the canonical typed forms and render
them via `ToSql` without needing in-DB allocation or `pub(crate)` access
to internal fields.

## Why this exists

`catalog::TableDefinition` (and its siblings) are the canonical typed
representation of SurrealQL DDL. They have `impl ToSql` and they lower to
`sql::statements::DefineTableStatement` — but their constructor demands
runtime IDs (`namespace_id`, `database_id`, `table_id`) and fills cache
UUIDs that only make sense for in-DB state tracking. The struct fields
are `pub(crate)`, so external crates cannot use struct-literal
construction.

For external codegen tools that want to:

1. Build a typed `TableDefinition` representing a schema element,
2. Render it to SurrealQL via `ToSql::to_sql()`,
3. Never touch the actual database,

…the existing API is the wrong shape. This initiative adds:

- `new_for_ddl(...)` constructors that supply dummy zero IDs and
  `Uuid::now_v7()` cache timestamps, suitable for "I'm just rendering
  this" use cases. The dummy IDs do not appear in the DDL output
  (verified in tests).
- `with_*(value)` chainable setters for the DDL-meaningful fields
  (`schemafull`, `drop`, `comment`, `table_type`, etc.). Encapsulates
  the `pub(crate)` fields behind a builder pattern.

## Downstream consumer

The first consumer is OpenProject nexgen's `op-surreal-ast` crate
(`AdaWorldAPI/openproject-nexgen-rs`), which currently mirrors the
catalog layout in its own structs (C16a). Once these constructors land
upstream-here, nexgen's C16c sprint adds `From<op_surreal_ast::*> for
catalog::*` impls and switches `op-codegen-projection` to render via the
canonical path — dropping the mirrored layout.

## Scope of *this* sprint (C16b)

In-scope:
- `TableDefinition`: `new_for_ddl` + setters for `schemafull`, `drop`,
  `comment`, `table_type`, `view`, `permissions`, `changefeed`
- `FieldDefinition`: `new_for_ddl` + setters for `field_kind`,
  `flexible`, `readonly`, `value`, `assert`, `computed`, `comment`,
  `reference`. **Not** setters for the three `Permission` slots —
  `Permission` is `pub(crate)`, the auth sprint will revisit.
- `IndexDefinition`: `new_for_ddl` (defaults to `Index::Idx`, non-unique)
  + setter for `comment`. **Not** a setter for `index` (Index is
  `pub(crate)`); convenience constructors for `Uniq` / `FullText` etc.
  follow in dedicated sprints.

Out-of-scope:
- Flipping any `pub(crate)` to `pub`. The setter pattern preserves the
  encapsulation deliberately.
- `EventDefinition`, `ViewDefinition`, `Relation` builder ergonomics —
  follow-up sprints.
- Lance backend (`kvs/lance/`) — separate initiative.

## Files touched

- `surrealdb/core/src/catalog/table.rs` — new `impl TableDefinition`
  block at the bottom, holding only DDL constructors/setters.
- `surrealdb/core/src/catalog/schema/field.rs` — same pattern.
- `surrealdb/core/src/catalog/schema/index.rs` — same pattern.

## Tests

Inline `#[cfg(test)] mod ddl_builder_tests` in each file. Pattern:
build via `new_for_ddl().with_*(…)`, build the same struct via raw
`pub(crate)` literal (still possible inside the crate), assert
`to_sql()` outputs match byte-for-byte.

## Status

Active. Initial commit: see `.claude/board/AGENT_LOG.md` (`c16b-*` tag).
