use revision::revisioned;
use surrealdb_types::{SqlFormat, ToSql};

use super::Permission;
use crate::catalog::auth::AuthLimit;
use crate::expr::reference::Reference;
use crate::expr::statements::info::InfoStructure;
use crate::expr::{Expr, Idiom, Kind};
use crate::kvs::impl_kv_value_revisioned;
use crate::sql::{self, DefineFieldStatement};
use crate::val::{TableName, Value};

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub(crate) enum DefineDefault {
	#[default]
	None,
	Always(Expr),
	Set(Expr),
}

/// Dependency metadata for a computed field.
///
/// Tracks which same-table fields a computed expression references, and whether
/// the static analysis was able to fully determine all dependencies.
#[revisioned(revision = 1)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ComputedDeps {
	/// Known same-table field names this computed field depends on.
	pub fields: Vec<String>,
	/// Whether static analysis could fully determine all dependencies.
	///
	/// When `false`, the expression contains opaque constructs (subqueries, params,
	/// graph traversals, etc.) that could access arbitrary fields at runtime.
	/// If such a field is needed by a query, ALL computed fields must be evaluated.
	pub is_complete: bool,
}

#[revisioned(revision = 3)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct FieldDefinition {
	// TODO: Needs to be it's own type.
	// Idiom::Value/Idiom::Start are for example not allowed.
	pub(crate) name: Idiom,
	pub(crate) table: TableName,
	// TODO: Optionally also be a seperate type from expr::Kind
	pub(crate) field_kind: Option<Kind>,
	pub(crate) flexible: bool,
	pub(crate) readonly: bool,
	pub(crate) value: Option<Expr>,
	pub(crate) assert: Option<Expr>,
	pub(crate) computed: Option<Expr>,
	pub(crate) default: DefineDefault,

	pub(crate) select_permission: Permission,
	pub(crate) create_permission: Permission,
	pub(crate) update_permission: Permission,

	pub(crate) comment: Option<String>,
	pub(crate) reference: Option<Reference>,

	/// The auth limit of the API.
	#[revision(start = 2, default_fn = "default_auth_limit")]
	pub(crate) auth_limit: AuthLimit,

	/// Pre-computed dependency metadata for computed fields.
	/// `None` for non-computed fields or legacy definitions (pre-revision 3).
	/// When `None` on a computed field, deps are extracted on-the-fly at query time.
	#[revision(start = 3, default_fn = "default_computed_deps")]
	pub(crate) computed_deps: Option<ComputedDeps>,
}

impl FieldDefinition {
	// This was pushed in after the first beta, so we need to add auth_limit to structs in a
	// non-breaking way
	fn default_auth_limit(_revision: u16) -> Result<AuthLimit, revision::Error> {
		Ok(AuthLimit::new_no_limit())
	}

	fn default_computed_deps(_revision: u16) -> Result<Option<ComputedDeps>, revision::Error> {
		Ok(None)
	}
}
impl_kv_value_revisioned!(FieldDefinition);

impl FieldDefinition {
	pub fn to_sql_definition(&self) -> DefineFieldStatement {
		DefineFieldStatement {
			kind: sql::statements::define::DefineKind::Default,
			name: Expr::Idiom(self.name.clone()).into(),
			what: sql::Expr::Table(self.table.clone().into_string()),
			field_kind: self.field_kind.clone().map(|x| x.into()),
			flexible: self.flexible,
			readonly: self.readonly,
			value: self.value.clone().map(|x| x.into()),
			assert: self.assert.clone().map(|x| x.into()),
			computed: self.computed.clone().map(|x| x.into()),
			default: match &self.default {
				DefineDefault::None => sql::statements::define::DefineDefault::None,
				DefineDefault::Set(x) => {
					sql::statements::define::DefineDefault::Set(x.clone().into())
				}
				DefineDefault::Always(x) => {
					sql::statements::define::DefineDefault::Always(x.clone().into())
				}
			},
			permissions: sql::Permissions {
				select: self.select_permission.to_sql_definition(),
				create: self.create_permission.to_sql_definition(),
				update: self.update_permission.to_sql_definition(),
				delete: sql::Permission::Full,
			},
			comment: self
				.comment
				.clone()
				.map(|x| sql::Expr::Literal(sql::Literal::String(x)))
				.unwrap_or(sql::Expr::Literal(sql::Literal::None)),
			reference: self.reference.clone().map(|x| x.into()),
		}
	}
}

impl InfoStructure for FieldDefinition {
	fn structure(self) -> Value {
		Value::from(map! {
			"name".to_string() => self.name.structure(),
			"table".to_string() => Value::String(self.table.into_string()),
			"kind".to_string(), if let Some(v) = self.field_kind => v.structure(),
			"flexible".to_string(), if self.flexible => true.into(),
			"value".to_string(), if let Some(v) = self.value => v.structure(),
			"assert".to_string(), if let Some(v) = self.assert => v.structure(),
			"computed".to_string(), if let Some(v) = self.computed => v.structure(),
			"default_always".to_string(), if matches!(&self.default, DefineDefault::Always(_) | DefineDefault::Set(_)) => Value::Bool(matches!(self.default,DefineDefault::Always(_))), // Only reported if DEFAULT is also enabled for this field
			"default".to_string(), if let DefineDefault::Always(v) | DefineDefault::Set(v) = self.default => v.structure(),
			"reference".to_string(), if let Some(v) = self.reference => v.structure(),
			"readonly".to_string() => self.readonly.into(),
			"permissions".to_string() => Value::from(map!{
				"select".to_string() => self.select_permission.structure(),
				"create".to_string() => self.create_permission.structure(),
				"update".to_string() => self.update_permission.structure(),
			}),
			"comment".to_string(), if let Some(v) = self.comment => v.into(),
		})
	}
}

impl ToSql for FieldDefinition {
	fn fmt_sql(&self, f: &mut String, fmt: SqlFormat) {
		self.to_sql_definition().fmt_sql(f, fmt)
	}
}

/// DDL-friendly constructor and chainable setters for [`FieldDefinition`].
///
/// External codegen tools build typed field definitions to render via
/// [`ToSql`] without an actual in-DB allocation. See the table-level
/// equivalent in `catalog::TableDefinition::new_for_ddl` and
/// `.claude/op-codegen-bridge/README.md` for the initiative context.
///
/// Not exposed: setters for `default` (uses `pub(crate) DefineDefault`),
/// the three `Permission` slots, `auth_limit`, and `computed_deps` — those
/// types are `pub(crate)` and follow in dedicated sprints (auth, computed
/// fields). For DDL emission of typical Rails-mapped schemas the slots
/// covered here (kind, assert, value, computed, comment, reference,
/// flexible, readonly) are sufficient.
//
// `dead_code` allowed: see the equivalent comment on the
// `TableDefinition` DDL-builder impl in `catalog/table.rs`.
#[allow(dead_code)]
impl FieldDefinition {
	/// Construct a [`FieldDefinition`] for **DDL emission only**.
	///
	/// All optional slots default to `None`; permissions default to
	/// `Permission::default()` (= `Full`); booleans default to `false`.
	/// Combine with the `with_*` builders below to fill DDL slots fluently.
	pub fn new_for_ddl(name: Idiom, table: TableName) -> Self {
		Self {
			name,
			table,
			..Default::default()
		}
	}

	/// Set `field_kind`. Returns `self` for chaining.
	#[must_use]
	pub fn with_kind(mut self, v: Option<Kind>) -> Self {
		self.field_kind = v;
		self
	}

	/// Set `flexible`. Returns `self` for chaining.
	#[must_use]
	pub fn with_flexible(mut self, v: bool) -> Self {
		self.flexible = v;
		self
	}

	/// Set `readonly`. Returns `self` for chaining.
	#[must_use]
	pub fn with_readonly(mut self, v: bool) -> Self {
		self.readonly = v;
		self
	}

	/// Set `value` (computed default expression). Returns `self` for
	/// chaining.
	#[must_use]
	pub fn with_value(mut self, v: Option<Expr>) -> Self {
		self.value = v;
		self
	}

	/// Set `assert` (validation expression). Returns `self` for chaining.
	#[must_use]
	pub fn with_assert(mut self, v: Option<Expr>) -> Self {
		self.assert = v;
		self
	}

	/// Set `computed` (virtual field expression). Returns `self` for
	/// chaining.
	#[must_use]
	pub fn with_computed(mut self, v: Option<Expr>) -> Self {
		self.computed = v;
		self
	}

	/// Set `comment`. Returns `self` for chaining.
	#[must_use]
	pub fn with_comment(mut self, v: Option<String>) -> Self {
		self.comment = v;
		self
	}

	/// Set `reference` (graph reference metadata). Returns `self` for
	/// chaining.
	#[must_use]
	pub fn with_reference(mut self, v: Option<Reference>) -> Self {
		self.reference = v;
		self
	}
}

#[cfg(test)]
mod ddl_builder_tests {
	//! C16b — DDL-friendly constructor + setters for FieldDefinition.
	//! See `.claude/op-codegen-bridge/README.md` for context.

	use std::str::FromStr;

	use surrealdb_types::ToSql;

	use super::*;
	use crate::expr::Idiom;
	use crate::val::TableName;

	fn idiom(s: &str) -> Idiom {
		Idiom::from_str(s).expect("test idiom literal must parse")
	}

	#[test]
	fn new_for_ddl_defaults_to_no_kind_and_no_constraints() {
		let f = FieldDefinition::new_for_ddl(idiom("name"), TableName::from("widget"));
		assert!(f.field_kind.is_none());
		assert!(f.value.is_none());
		assert!(f.assert.is_none());
		assert!(f.computed.is_none());
		assert!(!f.flexible);
		assert!(!f.readonly);
		assert!(f.comment.is_none());
		assert!(f.reference.is_none());
	}

	#[test]
	fn new_for_ddl_carries_name_and_table_through_to_sql() {
		let f = FieldDefinition::new_for_ddl(idiom("subject"), TableName::from("WorkPackage"));
		let sql = f.to_sql();
		assert!(sql.contains("subject"), "field name missing: {sql}");
		assert!(sql.contains("WorkPackage"), "table missing: {sql}");
	}

	#[test]
	fn with_kind_round_trips() {
		let f = FieldDefinition::new_for_ddl(idiom("count"), TableName::from("t"))
			.with_kind(Some(Kind::Int));
		assert!(matches!(f.field_kind, Some(Kind::Int)));
	}

	#[test]
	fn with_flexible_and_readonly_round_trip() {
		let f = FieldDefinition::new_for_ddl(idiom("c"), TableName::from("t"))
			.with_flexible(true)
			.with_readonly(true);
		assert!(f.flexible);
		assert!(f.readonly);
	}

	#[test]
	fn with_comment_round_trips() {
		let f = FieldDefinition::new_for_ddl(idiom("c"), TableName::from("t"))
			.with_comment(Some("hi".to_string()));
		assert_eq!(f.comment.as_deref(), Some("hi"));
	}

	#[test]
	fn builder_output_equals_struct_literal_output() {
		let raw = FieldDefinition {
			name: idiom("subject"),
			table: TableName::from("WorkPackage"),
			field_kind: Some(Kind::Int),
			flexible: true,
			readonly: false,
			value: None,
			assert: None,
			computed: None,
			default: DefineDefault::None,
			select_permission: Permission::default(),
			create_permission: Permission::default(),
			update_permission: Permission::default(),
			comment: Some("the subject".to_string()),
			reference: None,
			auth_limit: AuthLimit::new_no_limit(),
			computed_deps: None,
		};
		let built = FieldDefinition::new_for_ddl(idiom("subject"), TableName::from("WorkPackage"))
			.with_kind(Some(Kind::Int))
			.with_flexible(true)
			.with_comment(Some("the subject".to_string()));
		assert_eq!(raw.to_sql(), built.to_sql());
	}
}
