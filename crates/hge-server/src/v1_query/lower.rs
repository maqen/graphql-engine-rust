//! Shared context for lowering legacy DML to IR: table lookup and V2's permission checks.

use std::sync::Arc;

use hge_core::{Code, QualifiedTable, RoleName};
use hge_ir::{BoolExp, SelectPermission, TablePermissions};
use hge_schema::tables::{RelationshipInfo, TableInfo};
use hge_schema::{SchemaOptions, Tables};

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
}

impl PermKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PermKind::Select => "select",
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

    pub fn rel_target(&self, rel: &RelationshipInfo) -> Result<&'a Arc<TableInfo>, ApiError> {
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
}
