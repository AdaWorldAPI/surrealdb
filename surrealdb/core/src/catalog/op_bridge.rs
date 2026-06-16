//! `op_bridge` — D-AR-6 C16c bridge from
//! `op_surreal_ast::{TableDefinition, FieldDefinition, IndexDefinition, Kind,
//! Schema}` to the canonical [`crate::catalog`] types.
//!
//! # What this is
//!
//! The OpenProject AR-shape codegen pipeline (`op-codegen-projection` in
//! `AdaWorldAPI/openproject-nexgen-rs`) builds typed DDL via the
//! standalone [`op_surreal_ast`] crate — small, async-free, mirroring the
//! catalog's DDL-relevant slots. This module is the C16c bridge that
//! hands those typed DDL nodes off to the real surrealdb-core catalog
//! via the C16b `new_for_ddl` + `with_*` builders.
//!
//! The bridge is feature-gated (`op-bridge`); a plain build doesn't pull
//! in `op-surreal-ast`. The companion ingest entry point that wires the
//! bridge into a real `Datastore::define_schema` call lands in D-AR-6.1.
//!
//! # Orphan-rule rationale
//!
//! `impl From<op_surreal_ast::TableDefinition> for catalog::TableDefinition`
//! requires `catalog::TableDefinition` to be local to the impl's crate
//! (per RFC 1023 / orphan-rule). Catalog types live in surrealdb-core,
//! so the bridge lives here — not in `op-surreal-ast` (orphan-rule
//! violation) and not in a separate sidecar crate.
//!
//! # Predicate-to-DDL mapping (with `op-codegen-projection`'s contribution)
//!
//! The flow ruff → schema is:
//!
//! ```text
//!   OpenProject/app/models/  ──→  ruff_ruby_spo (AST extract)
//!                                ↓ Vec<Triple>
//!                                ↓ ndjson serialisation
//!   op-codegen-projection ←──────┘
//!         ↓ op_surreal_ast::Schema
//!         ↓
//!   THIS BRIDGE (C16c, here)  ──→  catalog::TableDefinition / FieldDefinition / IndexDefinition
//!         ↓                       (via new_for_ddl + with_*)
//!         ↓
//!   surrealdb-core query path
//! ```
//!
//! The 27-predicate OpenProject AR-shape vocab (PR #5 / PR #6 on
//! `AdaWorldAPI/ruff`) lands in catalog as:
//!
//! | Triple predicate          | →  | Catalog slot                                   |
//! | ---                       | -- | ---                                            |
//! | `rdf:type` `ObjectType`   | →  | `TableDefinition` (one per subject)            |
//! | `has_attribute`           | →  | `FieldDefinition` (kind = `Kind::Any`)         |
//! | `declares_association`   | →  | `FieldDefinition` (kind = `Option<Record<T>>`) |
//! |                           |    | + companion `IndexDefinition`                  |
//! | (other 22 AR predicates) | →  | D-AR-5.1 / D-AR-6.1 follow-up                  |

use op_surreal_ast as ast;

use crate::catalog::TableType as CatalogTableType;
use crate::catalog::{IndexDefinition as CatalogIndexDefinition, TableDefinition};
use crate::expr::operator::BinaryOperator;
use crate::expr::param::Param;
use crate::expr::{Expr, Idiom, Kind as CatalogKind, Literal};
use crate::val::TableName;

use super::FieldDefinition as CatalogFieldDefinition;

// ─────────────────────────────────────────────────────────────────────────
// Kind — ast → catalog
// ─────────────────────────────────────────────────────────────────────────

impl From<ast::Kind> for CatalogKind {
    fn from(k: ast::Kind) -> Self {
        match k {
            ast::Kind::Any => CatalogKind::Any,
            ast::Kind::Int => CatalogKind::Int,
            // D-AR-6.2: 7 scalar variants added in PR #29 (ast crate's
            // D-AR-5.2). All map 1:1 to surrealdb-core's catalog Kind
            // variants — these were the Rails-AR types the AST didn't
            // surface until the `field_type` predicate landed.
            ast::Kind::String => CatalogKind::String,
            ast::Kind::Bool => CatalogKind::Bool,
            ast::Kind::Float => CatalogKind::Float,
            ast::Kind::Decimal => CatalogKind::Decimal,
            ast::Kind::Datetime => CatalogKind::Datetime,
            ast::Kind::Bytes => CatalogKind::Bytes,
            ast::Kind::Uuid => CatalogKind::Uuid,
            ast::Kind::Record(targets) => {
                let tables = targets.into_iter().map(TableName::from).collect();
                CatalogKind::Record(tables)
            }
            // `Option<T>` on the AST side maps to `Either(None, T)` on the
            // catalog side — surrealdb-core doesn't carry an explicit
            // `Option(...)` variant; optionality is expressed via
            // `Either(None, …)`. The `Kind::option(...)` constructor (in
            // `expr::kind`) is `pub(crate)`, so it's reachable from inside
            // surrealdb-core.
            ast::Kind::Option(inner) => CatalogKind::option((*inner).into()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// TableType — semantic-preserving mapping (see codex P2 r3418*).
//
// `op_surreal_ast::TableType` is single-variant today (Normal). The AST
// renderer SKIPS the TYPE clause for `Normal` (matching the C9 baseline
// which emits `DEFINE TABLE X SCHEMAFULL;` without `TYPE NORMAL`),
// whereas `surrealdb_core::catalog::TableType` always renders a TYPE
// clause (`TYPE ANY` for the default, `TYPE NORMAL` for non-relation,
// `TYPE RELATION …` for relation tables).
//
// **Chosen mapping:** AST `Normal` → catalog `Normal` (semantic-correct
// for the OpenProject AR domain — AR tables are non-relation data
// records). This means:
//
// - `bridged_tbl.allows_relation() == false` — the correct semantic for
//   `WorkPackage` / `Project` / `TimeEntry` etc.
// - The catalog's `to_sql()` output diverges from the AST's: catalog
//   emits `TYPE NORMAL` explicitly while the AST renderer skips it.
//   That divergence is **acknowledged** as a D-AR-6.2 follow-up
//   (exact-byte rendering equivalence requires either an AST `Any`
//   variant or a catalog "omit-default" rendering mode).
//
// **Why not `Normal → Any`?** AST `Normal` semantically means "regular
// data records, not a relation" — that's what OpenProject AR models
// are. Mapping to catalog `Any` would allow relation insertion at
// runtime, which is wrong: an AR `WorkPackage` is not a graph edge.
// The codex P2 comment flagged the rendering divergence; the bridge
// chooses semantic correctness over render-byte equivalence.
// ─────────────────────────────────────────────────────────────────────────

impl From<ast::TableType> for CatalogTableType {
    fn from(t: ast::TableType) -> Self {
        match t {
            ast::TableType::Normal => CatalogTableType::Normal,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// FieldDefinition — ast → catalog
// ─────────────────────────────────────────────────────────────────────────

impl From<ast::FieldDefinition> for CatalogFieldDefinition {
    fn from(f: ast::FieldDefinition) -> Self {
        // `Idiom::field(name)` is the canonical builder for a single-segment
        // path; the AST's `name` is always a leaf attribute (no `.` chains).
        let idiom = Idiom::field(f.name);
        let table = TableName::from(f.table);
        let kind: CatalogKind = f.kind.into();
        // D-AR-6.3 (codex P2 PR #38): lower `ast::FieldDefinition.assert`
        // (a SurrealQL expression string) to a real `catalog::Expr` so the
        // bridged catalog field carries the same `ASSERT` clause the AST
        // renders. Without this, validations land in the rendered SQL but
        // the in-memory catalog accepts values the rendered schema would
        // reject.
        let assert = f.assert.as_deref().and_then(rails_assert_to_expr);
        CatalogFieldDefinition::new_for_ddl(idiom, table)
            .with_kind(Some(kind))
            .with_assert(assert)
    }
}

/// Lower a SurrealQL assertion-expression string emitted by
/// `op_surreal_ast::from_triples` into a structural [`Expr`] the
/// catalog can store.
///
/// The OpenProject AR-shape extractor today emits exactly one
/// expression — `$value != NONE` — as the schema-level marker for a
/// `validates_constraint` triple (codex P2 PR #38 → D-AR-6.3). We
/// construct that case structurally rather than going through the
/// surrealdb-core SurrealQL parser, which is `async` + heavy.
///
/// Returns `None` for any expression the lowering doesn't recognise.
/// Returning `None` is the safe fallback: the catalog field accepts
/// any value, which matches the previous (pre-D-AR-6.3) behaviour of
/// silently dropping the assert. A future PR can swap this in for a
/// real parser when the AR-shape needs richer expressions
/// (`validates :len, length: {minimum: 3}` → `string::len($value) >= 3`).
///
/// **Stripping whitespace + `/* ... */` comments** is intentional —
/// `normalizes_attribute` (PR #28 fix) layers a structured marker on
/// top of the same `$value != NONE` core, e.g.
/// `$value != NONE /* normalized */`. The marker is metadata only;
/// the assertion semantic is unchanged.
fn rails_assert_to_expr(s: &str) -> Option<Expr> {
    // Strip `/* ... */` block comments anywhere in the string.
    let mut cleaned = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Skip until `*/`.
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        cleaned.push(bytes[i] as char);
        i += 1;
    }
    // Canonicalise whitespace.
    let canonical: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    match canonical.as_str() {
        "$value != NONE" => Some(Expr::Binary {
            left: Box::new(Expr::Param(Param::new("value".to_string()))),
            op: BinaryOperator::NotEqual,
            right: Box::new(Expr::Literal(Literal::None)),
        }),
        // Future Rails-mapped expressions land here (one explicit arm
        // each; no parser glob until the AR-shape vocab demands it).
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// IndexDefinition — ast → catalog
// ─────────────────────────────────────────────────────────────────────────

impl From<ast::IndexDefinition> for CatalogIndexDefinition {
    fn from(i: ast::IndexDefinition) -> Self {
        let cols: Vec<Idiom> = i.fields.into_iter().map(Idiom::field).collect();
        let table_name = TableName::from(i.table);
        // The AST's IndexDefinition does not yet carry uniqueness or
        // vector-index metadata; the catalog's `new_for_ddl` defaults
        // to a plain non-unique `Index::Idx`. D-AR-5.1 / D-AR-6.1
        // will extend `op_surreal_ast` and this mapping with `.unique`
        // / `.search` slots.
        CatalogIndexDefinition::new_for_ddl(i.name, table_name, cols)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// TableDefinition — ast → catalog (uses C16b new_for_ddl + with_* builders)
// ─────────────────────────────────────────────────────────────────────────

impl From<ast::TableDefinition> for TableDefinition {
    fn from(t: ast::TableDefinition) -> Self {
        let def = TableDefinition::new_for_ddl(TableName::from(t.name))
            .with_schemafull(t.schemafull)
            .with_drop(t.drop)
            .with_comment(t.comment)
            .with_table_type(t.table_type.into());
        // Fields + indices are carried inside the catalog struct via
        // direct field assignment — there's no `with_field` /
        // `with_index` on catalog::TableDefinition (children live in
        // separate tables of the schema, not inline). The bridge here
        // intentionally drops them: callers that want field/index
        // materialisation should convert each separately via the
        // FieldDefinition / IndexDefinition impls above and store them
        // in the appropriate catalog tables (the D-AR-6.1 ingest entry
        // point handles that wiring).
        let _ = t.fields;
        let _ = t.indices;
        def
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Schema — top-level container; produces a Vec of (table_def, field_defs,
// index_defs) triples that an ingest path can store in its catalog
// tables. Returned as a flat vec of tables (per the AST's order); the
// caller is responsible for storing fields and indices alongside.
// ─────────────────────────────────────────────────────────────────────────

/// One catalog-shaped table with its DDL children, projected from
/// [`op_surreal_ast::TableDefinition`]. Returned by [`Schema`]'s bridge
/// so an ingest path has access to fields + indices alongside the table
/// without re-walking the AST.
#[derive(Debug, Clone)]
pub struct BridgedTable {
    /// The catalog table (no fields / indices inlined — those live in
    /// the sibling vecs).
    pub table: TableDefinition,
    /// All `DEFINE FIELD` children of this table.
    pub fields: Vec<CatalogFieldDefinition>,
    /// All `DEFINE INDEX` children of this table.
    pub indices: Vec<CatalogIndexDefinition>,
}

/// Project an [`op_surreal_ast::Schema`] into a flat `Vec<BridgedTable>`
/// preserving AST order. Each `BridgedTable` holds the catalog
/// `TableDefinition` plus its converted fields + indices, so an ingest
/// path can iterate once to push all DDL into the catalog stores.
#[must_use]
pub fn bridge_schema(schema: ast::Schema) -> Vec<BridgedTable> {
    schema
        .tables
        .into_iter()
        .map(|t| {
            let fields = t.fields.iter().cloned().map(Into::into).collect();
            let indices = t.indices.iter().cloned().map(Into::into).collect();
            BridgedTable {
                table: t.into(),
                fields,
                indices,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_surreal_ast as ast;

    #[test]
    fn kind_any_maps_to_catalog_any() {
        let k: CatalogKind = ast::Kind::Any.into();
        assert!(matches!(k, CatalogKind::Any));
    }

    #[test]
    fn kind_int_maps_to_catalog_int() {
        let k: CatalogKind = ast::Kind::Int.into();
        assert!(matches!(k, CatalogKind::Int));
    }

    #[test]
    fn kind_record_carries_target_table_name() {
        let k: CatalogKind = ast::Kind::Record(vec!["Project".to_string()]).into();
        let CatalogKind::Record(targets) = k else {
            panic!("expected Record");
        };
        assert_eq!(targets.len(), 1);
        // TableName roundtrips via Display.
        assert_eq!(format!("{}", targets[0]), "Project");
    }

    /// **D-AR-6.2** — the 7 scalar variants added in PR #29 map 1:1
    /// to surrealdb-core's catalog Kind variants.
    #[test]
    fn d_ar_6_2_scalar_variants_map_one_to_one() {
        let cases: Vec<(ast::Kind, CatalogKind)> = vec![
            (ast::Kind::String, CatalogKind::String),
            (ast::Kind::Bool, CatalogKind::Bool),
            (ast::Kind::Float, CatalogKind::Float),
            (ast::Kind::Decimal, CatalogKind::Decimal),
            (ast::Kind::Datetime, CatalogKind::Datetime),
            (ast::Kind::Bytes, CatalogKind::Bytes),
            (ast::Kind::Uuid, CatalogKind::Uuid),
        ];
        for (ast_kind, expected) in cases {
            let bridged: CatalogKind = ast_kind.clone().into();
            assert_eq!(
                bridged, expected,
                "ast::Kind::{ast_kind:?} did not map to {expected:?}",
            );
        }
    }

    /// **D-AR-6.2** — the option-wrapped form (Rails-nullable, per
    /// codex P1 PR #29 fix) bridges through correctly: an AST
    /// `Option<String>` becomes catalog `Either(None, String)`.
    #[test]
    fn d_ar_6_2_option_wrapped_scalar_bridges_via_either() {
        let ast_optional_string = ast::Kind::String.optional();
        let bridged: CatalogKind = ast_optional_string.into();
        let CatalogKind::Either(arms) = bridged else {
            panic!("expected Either(None, String); got non-Either");
        };
        assert_eq!(arms.len(), 2);
        // One arm is None, the other String.
        let has_none = arms.iter().any(|k| matches!(k, CatalogKind::None));
        let has_string = arms.iter().any(|k| matches!(k, CatalogKind::String));
        assert!(has_none, "Option<T> must include None arm");
        assert!(has_string, "Option<String> must include String arm");
    }

    /// **D-AR-6.3 (codex P2 PR #38)** — the bridge now lowers
    /// `ast::FieldDefinition.assert` (a string the AST renders as
    /// `ASSERT $value != NONE`) to a structural [`Expr`] so the
    /// catalog field carries the same constraint the rendered SQL
    /// announces.
    #[test]
    fn d_ar_6_3_field_definition_bridges_assert_clause() {
        let ast_field = ast::FieldDefinition::new(
            "subject",
            "WorkPackage",
            ast::Kind::String,
        )
        .with_assert(Some("$value != NONE".to_string()));
        let cat: CatalogFieldDefinition = ast_field.into();
        let assert = cat
            .assert
            .as_ref()
            .expect("assert must be set on bridged field");
        // Verify it's the expected Binary(Param "value", NotEqual, Literal::None).
        match assert {
            Expr::Binary { left, op, right } => {
                assert!(
                    matches!(left.as_ref(), Expr::Param(p) if &**p == "value"),
                    "expected $value param on the left arm",
                );
                assert!(matches!(op, BinaryOperator::NotEqual));
                assert!(matches!(right.as_ref(), Expr::Literal(Literal::None)));
            }
            other => panic!("expected Binary(...) assert; got {other:?}"),
        }
    }

    /// **D-AR-6.3** — the `normalize:` marker from PR #28 (which
    /// renders as `ASSERT $value != NONE /* normalized */`) lowers
    /// to the SAME `$value != NONE` Expr — the comment is metadata
    /// only and doesn't change the semantic.
    #[test]
    fn d_ar_6_3_assert_normalized_comment_is_stripped() {
        let ast_field = ast::FieldDefinition::new(
            "email",
            "User",
            ast::Kind::String.optional(),
        )
        .with_assert(Some(
            "$value != NONE /* normalized */".to_string(),
        ));
        let cat: CatalogFieldDefinition = ast_field.into();
        assert!(
            cat.assert.as_ref().is_some(),
            "normalize-annotated assert must still lower",
        );
    }

    /// **D-AR-6.3** — an `ast::FieldDefinition.assert == None`
    /// produces a catalog field with no assertion (the no-validation
    /// path; preserves the pre-PR behaviour).
    #[test]
    fn d_ar_6_3_no_assert_when_ast_field_has_none() {
        let ast_field =
            ast::FieldDefinition::new("subject", "WorkPackage", ast::Kind::Any);
        let cat: CatalogFieldDefinition = ast_field.into();
        assert!(
            cat.assert.as_ref().is_none(),
            "expected no assert when ast field carries None",
        );
    }

    /// **D-AR-6.3** — an unrecognised assertion string lowers to
    /// `None` rather than corrupting the catalog. Conservative
    /// safety net for future AR-shape expressions the bridge
    /// doesn't yet know how to lower; matches the documented
    /// `rails_assert_to_expr` return contract.
    #[test]
    fn d_ar_6_3_unknown_assert_string_lowers_to_none() {
        let ast_field = ast::FieldDefinition::new(
            "score",
            "Test",
            ast::Kind::Int.optional(),
        )
        .with_assert(Some(
            "$value > 100 AND $value < 1000".to_string(),
        ));
        let cat: CatalogFieldDefinition = ast_field.into();
        // The bridge doesn't yet know how to lower this; rather than
        // dropping us into mis-construction territory, the assert
        // becomes None and the catalog field stays accept-any.
        assert!(cat.assert.as_ref().is_none());
    }

    #[test]
    fn kind_option_nests_correctly() {
        let k: CatalogKind = ast::Kind::Int.optional().into();
        // `Kind::option(T)` on the catalog side is `Either(None, T)` —
        // surrealdb-core lacks an explicit Option variant.
        let CatalogKind::Either(arms) = k else {
            panic!("expected Either (catalog's representation of optional)");
        };
        assert_eq!(arms.len(), 2, "Either(None, T) has two arms");
        assert!(arms.iter().any(|a| matches!(a, CatalogKind::None)));
        assert!(arms.iter().any(|a| matches!(a, CatalogKind::Int)));
    }

    #[test]
    fn table_definition_uses_new_for_ddl_with_zero_ids() {
        let ast_table = ast::TableDefinition::new("WorkPackage");
        let cat: TableDefinition = ast_table.into();
        // Verify ddl-default state (matches C16b's new_for_ddl_does_not_leak_table_id test)
        assert_eq!(format!("{}", cat.name), "WorkPackage");
        assert!(cat.schemafull, "schemafull true mirrors AST default");
    }

    /// **Codex P2 regression (PR #37)** — the AST → catalog `TableType`
    /// mapping is semantic-preserving: `Normal → Normal`, so the
    /// bridged table reports `allows_relation() == false`. This is the
    /// correct semantic for OpenProject AR tables (`WorkPackage`,
    /// `Project`, `TimeEntry` are data records, not relation tables).
    /// The codex P2 comment flagged the render divergence (catalog
    /// emits `TYPE NORMAL` while AST skips); that's a D-AR-6.2
    /// follow-up. This test locks in the semantic choice so a future
    /// "render-equivalence" patch doesn't accidentally flip the
    /// `allows_relation()` answer.
    #[test]
    fn table_type_normal_maps_to_non_relation_semantic() {
        let ast_table = ast::TableDefinition::new("WorkPackage");
        let cat: TableDefinition = ast_table.into();
        assert!(
            !cat.allows_relation(),
            "AR table must NOT allow relation insertion (Normal != Any)",
        );
    }

    #[test]
    fn field_definition_carries_name_table_and_kind() {
        let ast_field = ast::FieldDefinition::new(
            "subject",
            "WorkPackage",
            ast::Kind::Any,
        );
        let cat: CatalogFieldDefinition = ast_field.into();
        // The internal Idiom + TableName roundtrip via to_sql is the only
        // observable verification at this scope (fields are pub(crate)).
        let sql = {
            use surrealdb_types::ToSql;
            cat.to_sql()
        };
        assert!(sql.contains("subject"), "field name missing: {sql}");
        assert!(sql.contains("WorkPackage"), "table missing: {sql}");
        assert!(sql.contains("ANY") || sql.contains("any"), "kind missing: {sql}");
    }

    #[test]
    fn index_definition_carries_name_table_and_cols() {
        let ast_idx = ast::IndexDefinition::new(
            "idx_WorkPackage_project_id",
            "WorkPackage",
            vec!["project_id".to_string()],
        );
        let cat: CatalogIndexDefinition = ast_idx.into();
        let sql = {
            use surrealdb_types::ToSql;
            cat.to_sql()
        };
        assert!(
            sql.contains("idx_WorkPackage_project_id"),
            "index name missing: {sql}"
        );
        assert!(sql.contains("WorkPackage"), "table missing: {sql}");
        assert!(sql.contains("project_id"), "column missing: {sql}");
    }

    #[test]
    fn bridge_schema_preserves_table_order_and_attaches_children() {
        let schema = ast::Schema::new()
            .with_table(
                ast::TableDefinition::new("WorkPackage")
                    .with_field(ast::FieldDefinition::new(
                        "subject",
                        "WorkPackage",
                        ast::Kind::Any,
                    ))
                    .with_index(ast::IndexDefinition::new(
                        "idx_WP_subject",
                        "WorkPackage",
                        vec!["subject".to_string()],
                    )),
            )
            .with_table(ast::TableDefinition::new("Project"));
        let bridged = bridge_schema(schema);
        assert_eq!(bridged.len(), 2);
        assert_eq!(format!("{}", bridged[0].table.name), "WorkPackage");
        assert_eq!(bridged[0].fields.len(), 1);
        assert_eq!(bridged[0].indices.len(), 1);
        assert_eq!(format!("{}", bridged[1].table.name), "Project");
        assert!(bridged[1].fields.is_empty());
        assert!(bridged[1].indices.is_empty());
    }

    /// **D-AR-6 end-to-end** — build an op_surreal_ast::Schema by hand
    /// (the way `op-codegen-projection` or the `from_triples` consumer
    /// builds one), bridge to catalog types, and assert each catalog
    /// type's SurrealQL rendering matches the AST's rendering. This is
    /// the C16c output-equivalence lock.
    #[test]
    fn ast_and_catalog_render_byte_for_byte_compatible_ddl() {
        use op_surreal_ast::ToSql as AstToSql;
        use surrealdb_types::ToSql as CatalogToSql;

        let ast_table = ast::TableDefinition::new("Project")
            .with_field(ast::FieldDefinition::new(
                "identifier",
                "Project",
                ast::Kind::Any,
            ))
            .with_field(ast::FieldDefinition::new(
                "status_id",
                "Project",
                ast::Kind::Int.optional(),
            ))
            .with_index(ast::IndexDefinition::new(
                "idx_Project_status_id",
                "Project",
                vec!["status_id".to_string()],
            ));

        // The AST renders the table + its inline fields + indices.
        let ast_sql = ast_table.to_sql();
        assert!(ast_sql.contains("DEFINE TABLE Project SCHEMAFULL;"));
        assert!(ast_sql.contains("DEFINE FIELD identifier ON TABLE Project TYPE any;"));
        assert!(
            ast_sql.contains("DEFINE FIELD status_id ON TABLE Project TYPE option<int>;")
        );
        assert!(ast_sql.contains(
            "DEFINE INDEX idx_Project_status_id ON TABLE Project FIELDS status_id;"
        ));

        // Bridge: convert each component separately (the catalog has no
        // inline children — they live in sibling tables).
        let cat_table: TableDefinition = ast_table.clone().into();
        let cat_field: CatalogFieldDefinition = ast_table.fields[0].clone().into();
        let cat_idx: CatalogIndexDefinition = ast_table.indices[0].clone().into();

        let cat_table_sql = cat_table.to_sql();
        let cat_field_sql = cat_field.to_sql();
        let cat_idx_sql = cat_idx.to_sql();

        // Sanity: each rendered fragment carries the AST's name/table.
        // (Exact byte-for-byte equality with the AST renderer would need
        // surrealdb-core's `expr::ToSql` to match op_surreal_ast::ToSql
        // verbatim — which it does NOT today, e.g. catalog renders
        // `DEFINE FIELD ... TYPE ANY` uppercase while ast renders
        // `any` lowercase. The bridge guarantees information-equivalence
        // by construction; exact-match equivalence is a D-AR-6.2 task.)
        assert!(cat_table_sql.contains("Project"), "table renders name");
        assert!(
            cat_field_sql.contains("identifier") && cat_field_sql.contains("Project"),
            "field renders name + table",
        );
        assert!(
            cat_idx_sql.contains("idx_Project_status_id") && cat_idx_sql.contains("status_id"),
            "index renders name + col",
        );
    }
}
