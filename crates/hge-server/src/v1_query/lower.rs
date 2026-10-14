//! Shared context for lowering legacy DML to IR: table lookup and V2's permission checks.

use std::sync::Arc;

use hge_core::{Code, QualifiedTable, RoleName, Session};
use hge_ir::{
    BoolExp, ColumnField, Field, MutationOutput, MutationResponseField, MutationTable,
    SelectPermission, TablePermissions, Value,
};
use hge_schema::mutation::access::{MutationAccess, mutation_access};
use hge_schema::tables::{ColumnInfo, RelationshipInfo, TableInfo};
use hge_schema::{SchemaOptions, Tables};
use serde_json::Value as Json;

use super::boolexp;
use super::json::{Path, display_table, err400};
use hge_metadata_api::ApiError;

/// The legacy DML API only works on the `default` Postgres source.
pub const DEFAULT_SOURCE: &str = "default";

pub struct Lowerer<'a> {
    pub tables: &'a Tables,
    pub source: Arc<str>,
    pub role: &'a RoleName,
    pub options: &'a SchemaOptions,
}

/// Kinds of table permission, as V2 names them in messages (mutations add theirs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermKind {
    Select,
    Insert,
    Update,
    Delete,
}

impl PermKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PermKind::Select => "select",
            PermKind::Insert => "insert",
            PermKind::Update => "update",
            PermKind::Delete => "delete",
        }
    }
}

impl<'a> Lowerer<'a> {
    pub fn new(
        tables: &'a Tables,
        source: &str,
        role: &'a RoleName,
        options: &'a SchemaOptions,
    ) -> Self {
        Lowerer {
            tables,
            source: Arc::from(source),
            role,
            options,
        }
    }

    pub fn table(&self, name: &QualifiedTable) -> Option<&'a Arc<TableInfo>> {
        self.tables.get(&self.source, name)
    }

    /// `table "t" does not exist` (V2's `askTableInfoSource`).
    pub fn require_table(
        &self,
        name: &QualifiedTable,
        path: &Path,
    ) -> Result<&'a Arc<TableInfo>, ApiError> {
        self.table(name).ok_or_else(|| {
            err400(
                Code::NotExists,
                format!("table \"{}\" does not exist", display_table(name)),
                path,
            )
        })
    }

    /// The relationship's target table. V2 fails with a 500 naming the function (`context`)
    /// on relationships onto native queries.
    pub fn rel_target(
        &self,
        rel: &RelationshipInfo,
        context: &str,
    ) -> Result<&'a Arc<TableInfo>, ApiError> {
        if rel.native_query.is_some() {
            return Err(ApiError::from(
                hge_core::Error::new(hge_core::Code::Unexpected, "Internal Server Error")
                    .with_internal(
                        serde_json::json!({ "error": format!("{context} RelTargetNativeQuery") }),
                    ),
            ));
        }
        self.tables.get(&rel.source, &rel.target).ok_or_else(|| {
            ApiError::from(hge_core::Error::unexpected(format!(
                "foreign table \"{}\" does not exist",
                display_table(&rel.target)
            )))
        })
    }

    fn permissions(&self, table: &'a TableInfo) -> Option<&'a TablePermissions> {
        hge_schema::permissions::table_permissions(table, self.role)
    }

    /// The role's select permission, or V2's `select on "t" for role "r" is not allowed. `.
    pub fn select_permission(
        &self,
        table: &'a TableInfo,
        path: &Path,
    ) -> Result<&'a SelectPermission, ApiError> {
        self.permissions(table)
            .and_then(|p| p.select.as_ref())
            .ok_or_else(|| self.not_allowed(PermKind::Select, table, path))
    }

    pub fn not_allowed(&self, kind: PermKind, table: &TableInfo, path: &Path) -> ApiError {
        err400(
            Code::PermissionDenied,
            format!(
                "{} on \"{}\" for role \"{}\" is not allowed. ",
                kind.as_str(),
                display_table(&table.name),
                self.role
            ),
            path,
        )
    }

    /// The select permission on a relationship's target table (V2's `fetchRelDet`).
    pub fn relationship_permission(
        &self,
        rel: &RelationshipInfo,
        target: &'a TableInfo,
        path: &Path,
    ) -> Result<&'a SelectPermission, ApiError> {
        self.permissions(target)
            .and_then(|p| p.select.as_ref())
            .ok_or_else(|| {
                err400(
                    Code::PermissionDenied,
                    format!(
                        "role \"{}\" does not have permission to read relationship \"{}\"; no permission on table \"{}\"",
                        self.role,
                        rel.name,
                        display_table(&target.name)
                    ),
                    path,
                )
            })
    }

    /// V2's `checkPermOnCol`: the column must be in `allowed`.
    pub fn check_column(
        &self,
        kind: PermKind,
        allowed: &[String],
        column: &str,
        path: &Path,
    ) -> Result<(), ApiError> {
        if allowed.iter().any(|c| c == column) {
            return Ok(());
        }
        let message = if self.role.is_admin() {
            format!("no such column exists: \"{column}\"")
        } else {
            format!(
                "role \"{}\" does not have permission to {} column \"{column}\"",
                self.role,
                kind.as_str()
            )
        };
        Err(err400(Code::PermissionDenied, message, path))
    }

    pub fn check_select_column(
        &self,
        perm: &SelectPermission,
        column: &str,
        path: &Path,
    ) -> Result<(), ApiError> {
        self.check_column(PermKind::Select, &perm.columns, column, path)
    }

    /// V2's `validateHeaders`: session variables used by a permission filter must be present.
    pub fn check_session_variables(
        &self,
        filter: &BoolExp,
        session: &hge_core::Session,
        path: &Path,
    ) -> Result<(), ApiError> {
        let mut names = vec![];
        boolexp::session_variables(filter, &mut names);
        for name in names {
            if session.variables.get(&name).is_none() {
                return Err(err400(
                    Code::NotFound,
                    format!("\"{name}\" header is expected but not found"),
                    path,
                ));
            }
        }
        Ok(())
    }

    /// Whether a column's value is returned as a string
    /// (`HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`).
    pub fn stringify(&self, ty: &hge_core::ColumnType) -> bool {
        self.options.stringify_numeric_types && hge_schema::scalars::is_big_num(ty)
    }

    // --- mutations -----------------------------------------------------------------------

    /// The role's insert/update/delete permissions on `table`. `backend_only` permissions
    /// count only for requests that use them (`x-hasura-use-backend-only-permissions`), as
    /// on GraphQL; V2's legacy DML ignores the flag, which would let any client use them.
    pub fn mutation_access(&self, table: &TableInfo, session: &Session) -> MutationAccess {
        let access = mutation_access(table, self.role);
        if session.backend_only {
            return access;
        }
        MutationAccess {
            insert: access.insert.filter(|a| !a.backend_only),
            update: access.update.filter(|a| !a.backend_only),
            delete: access.delete.filter(|a| !a.backend_only),
        }
    }

    /// V2's `mutableView`: `view "v" is not insertable`.
    pub fn require_mutable(
        &self,
        table: &TableInfo,
        kind: PermKind,
        path: &Path,
    ) -> Result<(), ApiError> {
        let m = &table.mutability;
        let (allowed, operation) = match kind {
            PermKind::Insert => (m.insertable, "insertable"),
            PermKind::Update => (m.updatable, "updatable"),
            PermKind::Delete => (m.deletable, "deletable"),
            PermKind::Select => (true, ""),
        };
        if allowed {
            return Ok(());
        }
        Err(err400(
            Code::NotSupported,
            format!("view \"{}\" is not {operation}", display_table(&table.name)),
            path,
        ))
    }

    /// The select permission `returning` (or an update/delete `where`) needs, with V2's
    /// explanation appended to the error.
    pub fn select_permission_for(
        &self,
        table: &'a TableInfo,
        path: &Path,
        reason: &str,
    ) -> Result<&'a SelectPermission, ApiError> {
        self.select_permission(table, path).map_err(|mut e| {
            e.error.message.push_str(reason);
            e
        })
    }

    /// V2's `validateHeaders` over a mutation permission: every session variable its
    /// filters, checks and presets read must be in the request.
    pub fn require_session_variables<'e>(
        &self,
        exps: impl IntoIterator<Item = &'e BoolExp>,
        values: impl IntoIterator<Item = &'e Value>,
        session: &Session,
        path: &Path,
    ) -> Result<(), ApiError> {
        let mut names = vec![];
        for exp in exps {
            boolexp::session_variables(exp, &mut names);
        }
        for v in values {
            if let Value::Session { name, .. } = v
                && !names.contains(name)
            {
                names.push(name.clone());
            }
        }
        for name in names {
            if session.variables.get(&name).is_none() {
                return Err(err400(
                    Code::NotFound,
                    format!("\"{name}\" header is expected but not found"),
                    path,
                ));
            }
        }
        Ok(())
    }

    /// V2's `checkRetCols`: every column needs select permission (checked first, for all
    /// columns), then must be a column. Values are redacted for inherited roles as in
    /// `select` (V2's legacy `returning` does not redact).
    pub fn returning(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        columns: &[String],
        session: &Session,
        path: &Path,
    ) -> Result<Vec<(String, Field)>, ApiError> {
        for c in columns {
            self.check_select_column(perm, c, path)?;
        }
        let mut fields = Vec::with_capacity(columns.len());
        for c in columns {
            let info = column_info(
                table,
                c,
                "Relationships can't be used in \"returning\".",
                path,
            )?;
            fields.push((
                c.clone(),
                Field::Column(ColumnField {
                    column: c.clone(),
                    ty: info.ty.clone(),
                    stringify: self.stringify(&info.ty),
                    json_path: None,
                    // A session variable missing from a redaction condition reads as NULL
                    // (the column is redacted), as in `select`.
                    redact: perm.redaction(c).map(|cond| {
                        let mut cond = cond.clone();
                        super::boolexp::nullify_missing_session_variables(&mut cond, session);
                        Box::new(cond)
                    }),
                }),
            ));
        }
        Ok(fields)
    }
}

/// V2's `askColInfo`: `column "c" does not exist`, or a relationship used as a column.
pub fn column_info<'t>(
    table: &'t TableInfo,
    column: &str,
    relationship_message: &str,
    path: &Path,
) -> Result<&'t ColumnInfo, ApiError> {
    if let Some(info) = table.columns.get(column) {
        return Ok(info);
    }
    if table.relationship_by_name(column).is_some() {
        return Err(err400(
            Code::UnexpectedPayload,
            format!(
                "expecting a database column; but, \"{column}\" is a relationship; {relationship_message}"
            ),
            path,
        ));
    }
    Err(err400(
        Code::NotExists,
        format!("column \"{column}\" does not exist"),
        path,
    ))
}

/// V2's `mkDefaultMutFlds`: `affected_rows`, plus `returning` when requested.
pub fn mutation_output(returning: Option<Vec<(String, Field)>>) -> MutationOutput {
    let mut fields = vec![(
        "affected_rows".to_owned(),
        MutationResponseField::AffectedRows,
    )];
    if let Some(r) = returning {
        fields.push(("returning".to_owned(), MutationResponseField::Returning(r)));
    }
    MutationOutput::Multi(fields)
}

pub fn mutation_table(table: &TableInfo) -> MutationTable {
    MutationTable {
        name: table.name.clone(),
        columns: table
            .columns
            .values()
            .map(|c| (c.name.clone(), c.ty.clone()))
            .collect(),
    }
}

/// A value for a column written by legacy DML. V2 sends these as binary prepared values; a
/// `numeric` then gets a scale that is a multiple of 4 (postgresql-binary's base-10000
/// encoding of the `Scientific`): `1.1` is stored as `1.1000`, `3` as `3`. Other values are
/// read like `where` literals.
pub fn dml_value(v: &Json, ty: &hge_core::ColumnType, path: &Path) -> Result<Value, ApiError> {
    if let Json::Number(n) = v
        && ty.name == "numeric"
        && !ty.is_array()
        && let Some(text) = binary_numeric_text(&n.to_string())
    {
        return Ok(Value::literal(text, ty.clone()));
    }
    boolexp::literal(v, ty, path)
}

/// The decimal text of the number written `s`, with the scale V2's binary encoding gives it:
/// the number of fractional digits, rounded up to a multiple of 4.
fn binary_numeric_text(s: &str) -> Option<String> {
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (
            &s[..i],
            s[i + 1..].trim_start_matches('+').parse::<i64>().ok()?,
        ),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    // value = digits * 10^exp10
    let mut digits = format!("{int}{frac}");
    let exp10 = exp.checked_sub(frac.len() as i64)?;
    // Refuse absurd exponents rather than build huge strings.
    if exp10.abs() > 1000 {
        return None;
    }
    let scale = if exp10 >= 0 {
        digits.extend(std::iter::repeat_n('0', exp10 as usize));
        0
    } else {
        let scale = (-exp10 as usize).div_ceil(4) * 4;
        digits.extend(std::iter::repeat_n('0', scale - (-exp10) as usize));
        scale
    };
    if digits.len() <= scale {
        digits = format!("{}{digits}", "0".repeat(scale + 1 - digits.len()));
    }
    let (int, frac) = digits.split_at(digits.len() - scale);
    let int = match int.trim_start_matches('0') {
        "" => "0",
        i => i,
    };
    let sign = if negative && digits.chars().any(|c| c != '0') {
        "-"
    } else {
        ""
    };
    Some(if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    })
}

#[cfg(test)]
mod tests {
    use super::binary_numeric_text;

    #[test]
    fn numeric_scale_like_v2_binary_encoding() {
        for (input, text) in [
            ("3", "3"),
            ("15", "15"),
            ("1.1", "1.1000"),
            ("-1.1", "-1.1000"),
            ("0.5", "0.5000"),
            ("12.34567", "12.34567000"),
            ("1e-5", "0.00001000"),
            ("1.5e3", "1500"),
            ("1e2", "100"),
            ("0", "0"),
        ] {
            assert_eq!(binary_numeric_text(input).as_deref(), Some(text), "{input}");
        }
    }
}
