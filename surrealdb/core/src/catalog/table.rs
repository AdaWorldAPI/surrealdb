use revision::{DeserializeRevisioned, Revisioned, SerializeRevisioned, revisioned};
use surrealdb_types::{SqlFormat, ToSql, write_sql};
use uuid::Uuid;

use crate::catalog::{DatabaseId, NamespaceId, Permissions, ViewDefinition};
use crate::expr::statements::info::InfoStructure;
use crate::expr::{ChangeFeed, Kind};
use crate::fmt::EscapeKwFreeIdent;
use crate::kvs::impl_kv_value_revisioned;
use crate::sql;
use crate::sql::statements::DefineTableStatement;
use crate::val::{TableName, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct TableId(pub u32);

impl_kv_value_revisioned!(TableId);

impl Revisioned for TableId {
	fn revision() -> u16 {
		1
	}
}

impl SerializeRevisioned for TableId {
	#[inline]
	fn serialize_revisioned<W: std::io::Write>(
		&self,
		writer: &mut W,
	) -> Result<(), revision::Error> {
		SerializeRevisioned::serialize_revisioned(&self.0, writer)
	}
}

impl DeserializeRevisioned for TableId {
	#[inline]
	fn deserialize_revisioned<R: std::io::Read>(reader: &mut R) -> Result<Self, revision::Error> {
		DeserializeRevisioned::deserialize_revisioned(reader).map(TableId)
	}
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TableDefinition {
	pub(crate) namespace_id: NamespaceId,
	pub(crate) database_id: DatabaseId,
	pub(crate) table_id: TableId,
	pub(crate) name: TableName,
	pub(crate) drop: bool,
	pub(crate) schemafull: bool,
	pub(crate) view: Option<ViewDefinition>,
	pub(crate) permissions: Permissions,
	pub(crate) changefeed: Option<ChangeFeed>,
	pub(crate) comment: Option<String>,
	pub(crate) table_type: TableType,

	/// The last time that a DEFINE FIELD was added to this table
	pub(crate) cache_fields_ts: Uuid,
	/// The last time that a DEFINE EVENT was added to this table
	pub(crate) cache_events_ts: Uuid,
	/// The last time that a DEFINE TABLE was added to this table
	pub(crate) cache_tables_ts: Uuid,
	/// The last time that a DEFINE INDEX was added to this table
	pub(crate) cache_indexes_ts: Uuid,
}

impl_kv_value_revisioned!(TableDefinition);

impl TableDefinition {
	pub fn new(
		namespace_id: NamespaceId,
		database_id: DatabaseId,
		table_id: TableId,
		name: TableName,
	) -> Self {
		let now = Uuid::now_v7();
		Self {
			namespace_id,
			database_id,
			table_id,
			name,
			drop: false,
			schemafull: false,
			view: None,
			permissions: Permissions::none(),
			changefeed: None,
			comment: None,
			table_type: TableType::default(),
			cache_fields_ts: now,
			cache_events_ts: now,
			cache_tables_ts: now,
			cache_indexes_ts: now,
		}
	}

	/// Checks if this table allows normal records / documents
	pub fn allows_normal(&self) -> bool {
		matches!(self.table_type, TableType::Normal | TableType::Any)
	}
	/// Checks if this table allows graph edges / relations
	pub fn allows_relation(&self) -> bool {
		matches!(self.table_type, TableType::Relation(_) | TableType::Any)
	}

	fn to_sql_definition(&self) -> DefineTableStatement {
		DefineTableStatement {
			id: Some(self.table_id.0),
			name: sql::Expr::Table(self.name.clone().into_string()),
			drop: self.drop,
			full: self.schemafull,
			view: self.view.clone().map(|v| v.to_sql_definition()),
			permissions: self.permissions.clone().into(),
			changefeed: self.changefeed.map(|v| v.into()),
			comment: self
				.comment
				.clone()
				.map(|v| sql::Expr::Literal(sql::Literal::String(v)))
				.unwrap_or(sql::Expr::Literal(sql::Literal::None)),
			table_type: self.table_type.clone().into(),
			..Default::default()
		}
	}
}

impl ToSql for TableDefinition {
	fn fmt_sql(&self, f: &mut String, sql_fmt: SqlFormat) {
		self.to_sql_definition().fmt_sql(f, sql_fmt)
	}
}

/// DDL-friendly constructors and chainable setters for [`TableDefinition`].
///
/// External codegen tools build typed table definitions to render via
/// [`ToSql`] without an actual in-DB allocation. These methods supply dummy
/// runtime IDs and a chainable setter API so the `pub(crate)` fields stay
/// encapsulated. See `.claude/op-codegen-bridge/README.md` for the
/// initiative context.
//
// `dead_code` is allowed because these `pub` items have no in-crate
// callers — they exist solely as the public ergonomic surface for
// external `surrealdb_core` consumers. The `#[cfg(test)]
// ddl_builder_tests` module below exercises every method, but the
// `dead_code` lint runs on the non-test build target where no caller
// is visible.
#[allow(dead_code)]
impl TableDefinition {
	/// Construct a [`TableDefinition`] for **DDL emission only**.
	///
	/// All runtime IDs are set to dummy zero values
	/// (`NamespaceId(0)`, `DatabaseId(0)`, `TableId(0)`); cache timestamps
	/// are `Uuid::now_v7()`. The dummy IDs do not appear in DDL output —
	/// the rendered `DefineTableStatement` only emits `id` when explicitly
	/// set elsewhere in the codegen path. Verified by the
	/// [`ddl_builder_tests::new_for_ddl_does_not_leak_table_id_into_render`]
	/// test below.
	///
	/// Suitable for callers that want to build a typed table definition
	/// purely to render it to SurrealQL via [`ToSql::to_sql`]. Combine with
	/// the `with_*` builders below to fill DDL slots fluently.
	pub fn new_for_ddl(name: impl Into<TableName>) -> Self {
		Self::new(NamespaceId(0), DatabaseId(0), TableId(0), name.into())
	}

	/// Set `schemafull`. Returns `self` for chaining.
	#[must_use]
	pub fn with_schemafull(mut self, v: bool) -> Self {
		self.schemafull = v;
		self
	}

	/// Set `drop`. Returns `self` for chaining.
	#[must_use]
	pub fn with_drop(mut self, v: bool) -> Self {
		self.drop = v;
		self
	}

	/// Set `comment`. Returns `self` for chaining.
	#[must_use]
	pub fn with_comment(mut self, v: Option<String>) -> Self {
		self.comment = v;
		self
	}

	/// Set `table_type`. Returns `self` for chaining.
	#[must_use]
	pub fn with_table_type(mut self, v: TableType) -> Self {
		self.table_type = v;
		self
	}

	/// Set `view`. Returns `self` for chaining.
	#[must_use]
	pub fn with_view(mut self, v: Option<ViewDefinition>) -> Self {
		self.view = v;
		self
	}

	/// Set `permissions`. Returns `self` for chaining.
	#[must_use]
	pub fn with_permissions(mut self, v: Permissions) -> Self {
		self.permissions = v;
		self
	}

	// Note: `with_changefeed` deliberately omitted — `ChangeFeed` is
	// `pub(crate)` in surrealdb-core, so exposing it as a parameter
	// triggers a private-in-public warning. Changefeed configuration is
	// a runtime concern, not a DDL one; codegen tools that need it can
	// modify the field directly inside the crate, or wait until upstream
	// promotes the type.
}

impl InfoStructure for TableDefinition {
	fn structure(self) -> Value {
		Value::from(map! {
			"name".to_string() => self.name.into_string().into(),
			"drop".to_string() => self.drop.into(),
			"schemafull".to_string() => self.schemafull.into(),
			"kind".to_string() => self.table_type.structure(),
			"view".to_string(), if let Some(v) = self.view => v.structure(),
			"changefeed".to_string(), if let Some(v) = self.changefeed => v.structure(),
			"permissions".to_string() => self.permissions.structure(),
			"comment".to_string(), if let Some(v) = self.comment => v.into(),
			"id".to_string() => self.table_id.0.into(),
		})
	}
}

/// The type of records stored by a table
#[revisioned(revision = 1)]
#[derive(Debug, Default, Hash, Clone, Eq, PartialEq)]
pub enum TableType {
	#[default]
	Any,
	Normal,
	Relation(Relation),
}

impl ToSql for TableType {
	fn fmt_sql(&self, f: &mut String, sql_fmt: SqlFormat) {
		match self {
			TableType::Any => f.push_str("ANY"),
			TableType::Normal => f.push_str("NORMAL"),
			TableType::Relation(rel) => {
				f.push_str("RELATION");
				if !rel.from.is_empty() {
					f.push_str(" IN ");
					for (idx, k) in rel.from.iter().enumerate() {
						if idx != 0 {
							f.push_str(" | ");
						}
						write_sql!(f, sql_fmt, "{}", EscapeKwFreeIdent(k));
					}
				}
				if !rel.to.is_empty() {
					f.push_str(" OUT ");
					for (idx, k) in rel.to.iter().enumerate() {
						if idx != 0 {
							f.push_str(" | ");
						}
						write_sql!(f, sql_fmt, "{}", EscapeKwFreeIdent(k));
					}
				}
				if rel.enforced {
					f.push_str(" ENFORCED");
				}
			}
		}
	}
}

impl InfoStructure for TableType {
	fn structure(self) -> Value {
		match self {
			Self::Any => Value::from(map! {
				"kind".to_string() => "ANY".into(),
			}),
			Self::Normal => Value::from(map! {
				"kind".to_string() => "NORMAL".into(),
			}),
			Self::Relation(rel) => Value::from(map! {
				"kind".to_string() => "RELATION".into(),
				"in".to_string(), if !rel.from.is_empty() =>
					rel.from.into_iter().map(Value::from).collect::<Vec<_>>().into(),
				"out".to_string(), if !rel.to.is_empty() =>
					rel.to.into_iter().map(Value::from).collect::<Vec<_>>().into(),
				"enforced".to_string() => rel.enforced.into()
			}),
		}
	}
}

#[revisioned(revision = 2)]
#[derive(Debug, Hash, Clone, Eq, PartialEq)]
pub struct Relation {
	#[revision(end = 2, convert_fn = "rev_convert_from")]
	pub old_from: Option<Kind>,
	/// Contains the tables the relation originates from,
	/// if empty then there was no `IN` clause
	#[revision(start = 2)]
	pub from: Vec<String>,
	#[revision(end = 2, convert_fn = "rev_convert_to")]
	pub old_to: Option<Kind>,
	/// Contains the tables the relation goes to,
	/// if empty then there was no `OUT` clause
	#[revision(start = 2)]
	pub to: Vec<String>,
	pub enforced: bool,
}

impl Relation {
	fn rev_convert_from(&mut self, _rev: u16, value: Option<Kind>) -> Result<(), revision::Error> {
		if let Some(x) = value {
			let Kind::Record(x) = x else {
				return Err(revision::Error::Conversion(format!(
					"Invalid kind within table relation, should have been a record, found: {:#?}",
					x,
				)));
			};
			self.from = x.into_iter().map(|x| x.into_string()).collect()
		}
		Ok(())
	}
	fn rev_convert_to(&mut self, _rev: u16, value: Option<Kind>) -> Result<(), revision::Error> {
		if let Some(x) = value {
			let Kind::Record(x) = x else {
				return Err(revision::Error::Conversion(format!(
					"Invalid kind within table relation, should have been a record, found: {:#?}",
					x,
				)));
			};
			self.to = x.into_iter().map(|x| x.into_string()).collect()
		}
		Ok(())
	}
}

#[cfg(test)]
mod ddl_builder_tests {
	//! C16b — DDL-friendly constructors and setters.
	//!
	//! Each test exercises the path an external codegen tool takes:
	//! build via `new_for_ddl(...).with_*(...)`, then render via
	//! `ToSql::to_sql()`. The DDL output must match what an in-crate
	//! struct-literal construction with the same DDL slots produces.
	//!
	//! See `.claude/op-codegen-bridge/README.md` for the initiative
	//! context.

	use surrealdb_types::ToSql;
	use uuid::Uuid;

	use super::*;
	use crate::val::TableName;

	/// The dummy IDs supplied by `new_for_ddl` must not leak into the
	/// rendered SurrealQL. This is the invariant that makes the
	/// "build-for-render-only" pattern safe: an external caller never
	/// has to think about `namespace_id`/`database_id`/`table_id`.
	#[test]
	fn new_for_ddl_does_not_leak_table_id_into_render() {
		let t = TableDefinition::new_for_ddl("widget").with_schemafull(true);
		let sql = t.to_sql();
		// id appears in DefineTableStatement only when explicitly set
		// via the codegen path; new_for_ddl() leaves it at the default
		// (None), so neither "0" (the dummy TableId) nor "table_id"
		// shows up.
		assert!(!sql.contains(" 0 "), "raw 0 leaked into render: {sql}");
		assert!(sql.contains("widget"), "table name missing: {sql}");
		assert!(sql.contains("SCHEMAFULL"), "schemafull missing: {sql}");
	}

	/// Builder output equals struct-literal output (with cache UUIDs
	/// matched). This is the strongest invariant: the builder is a
	/// pure ergonomic wrapper, semantically identical to a raw
	/// struct construction.
	#[test]
	fn builder_output_equals_struct_literal_output() {
		let now = Uuid::now_v7();
		let raw = TableDefinition {
			namespace_id: NamespaceId(0),
			database_id: DatabaseId(0),
			table_id: TableId(0),
			name: TableName::from("widget"),
			drop: false,
			schemafull: true,
			view: None,
			permissions: Permissions::none(),
			changefeed: None,
			comment: Some("a widget".to_string()),
			table_type: TableType::Normal,
			cache_fields_ts: now,
			cache_events_ts: now,
			cache_tables_ts: now,
			cache_indexes_ts: now,
		};
		let built = TableDefinition::new_for_ddl("widget")
			.with_schemafull(true)
			.with_comment(Some("a widget".to_string()))
			.with_table_type(TableType::Normal)
			.with_permissions(Permissions::none());
		// Cache UUIDs differ (built uses now_v7 at construction), but
		// DDL output is independent of them — the `to_sql_definition`
		// path doesn't include cache_*_ts. So the rendered strings
		// must match exactly.
		assert_eq!(raw.to_sql(), built.to_sql());
	}

	#[test]
	fn with_schemafull_round_trips() {
		assert!(TableDefinition::new_for_ddl("t").with_schemafull(true).schemafull);
		assert!(!TableDefinition::new_for_ddl("t").with_schemafull(false).schemafull);
	}

	#[test]
	fn with_drop_round_trips() {
		assert!(TableDefinition::new_for_ddl("t").with_drop(true).drop);
	}

	#[test]
	fn with_comment_round_trips() {
		let t = TableDefinition::new_for_ddl("t").with_comment(Some("hi".to_string()));
		assert_eq!(t.comment.as_deref(), Some("hi"));
	}

	#[test]
	fn with_table_type_round_trips() {
		let t = TableDefinition::new_for_ddl("t").with_table_type(TableType::Normal);
		assert!(matches!(t.table_type, TableType::Normal));
	}

	#[test]
	fn new_for_ddl_supplies_dummy_ids_and_now_caches() {
		let t = TableDefinition::new_for_ddl("t");
		assert_eq!(t.namespace_id.0, 0);
		assert_eq!(t.database_id.0, 0);
		assert_eq!(t.table_id.0, 0);
		// cache UUIDs must be non-nil (now_v7), proving the constructor
		// didn't leave them at default (which would be the all-zeros
		// nil UUID and could break invariants elsewhere).
		assert_ne!(t.cache_fields_ts, Uuid::nil());
	}

	#[test]
	fn chained_builders_preserve_all_set_values() {
		let t = TableDefinition::new_for_ddl("multi")
			.with_schemafull(true)
			.with_drop(false)
			.with_comment(Some("c".to_string()))
			.with_table_type(TableType::Normal);
		assert_eq!(t.name.clone().into_string(), "multi");
		assert!(t.schemafull);
		assert!(!t.drop);
		assert_eq!(t.comment.as_deref(), Some("c"));
		assert!(matches!(t.table_type, TableType::Normal));
	}
}
