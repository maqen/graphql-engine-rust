//! Legacy `count`: `{"table", "distinct"?: [col], "where"?}` → `{"count": n}`.

use hge_core::{Code, QualifiedTable, Session};
use hge_ir::Count;
use serde_json::Value as Json;

use super::boolexp::nullify_missing_session_variables;
use super::json::{self, Obj, Path, err400};
use super::lower::Lowerer;
use hge_metadata_api::ApiError;

#[derive(Debug, Clone)]
pub struct CountQuery {
    pub source: String,
    pub table: QualifiedTable,
    distinct: Option<Vec<String>>,
    where_: Option<Json>,
    pub path: Path,
}

impl CountQuery {
    pub fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::new(v, path.clone(), "count query")?;
        Ok(CountQuery {
            table: json::table_name(o.req("table")?, &o.at("table"))?,
            source: o
                .string("source")?
                .unwrap_or_else(|| super::lower::DEFAULT_SOURCE.to_owned()),
            distinct: o.strings("distinct")?,
            where_: o.opt("where").cloned(),
            path: path.clone(),
        })
    }
}

impl Lowerer<'_> {
    /// V2's `validateCountQWith`.
    pub fn count(&self, q: &CountQuery, session: &Session) -> Result<Count, ApiError> {
        let table = self.require_table(&q.table, &q.path)?;
        let perm = self.select_permission(table, &q.path).map_err(|mut e| {
            e.error.message.push_str(
                "; \"count\" is only allowed if the role has \"select\" permissions on the table",
            );
            e
        })?;
        let distinct = q.distinct.clone().unwrap_or_default();
        let distinct_path = q.path.key("distinct");
        for (i, col) in distinct.iter().enumerate() {
            let col_path = distinct_path.index(i);
            self.check_select_column(perm, col, &col_path)?;
            // `COUNT(DISTINCT col)` reads the raw value; a column an inherited role sees only
            // on some rows would leak through it.
            if perm.redaction(col).is_some() {
                return Err(err400(
                    Code::PermissionDenied,
                    format!(
                        "column \"{col}\" is only partially visible to role \"{}\" and cannot be used in \"distinct\"",
                        self.role
                    ),
                    &col_path,
                ));
            }
            // Unreachable while permissions name only real columns, as in V2.
            if !table.columns.contains_key(col.as_str()) {
                let msg = if table.relationship_by_name(col).is_some() {
                    format!(
                        "expecting a database column; but, \"{col}\" is a relationship; Relationships can't be used in \"distinct\"."
                    )
                } else {
                    format!("column \"{col}\" does not exist")
                };
                return Err(err400(Code::UnexpectedPayload, msg, &col_path));
            }
        }
        let mut where_ = q
            .where_
            .as_ref()
            .map(|w| self.bool_exp(table, perm, w, &q.path.key("where")))
            .transpose()?;
        let mut permission_filter = perm.row_filter().cloned();
        // The filters may carry relationship permissions too (`where` through a relationship).
        for f in permission_filter.iter_mut().chain(where_.iter_mut()) {
            nullify_missing_session_variables(f, session);
        }
        Ok(Count {
            table: table.name.clone(),
            distinct,
            permission_filter,
            where_,
        })
    }
}
