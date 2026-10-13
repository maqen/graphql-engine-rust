//! Legacy `delete`: `{"table", "where", "returning"?}` →
//! `{"affected_rows": n, "returning"?: [...]}` (V2's `Hasura.RQL.DML.Delete`).

use hge_core::{QualifiedTable, Session};
use hge_ir::{BoolExp, Delete, Mutation};
use serde_json::Value as Json;

use super::boolexp::nullify_missing_session_variables;
use super::json::{self, Obj, Path};
use super::lower::{Lowerer, PermKind, mutation_output, mutation_table};
use hge_metadata_api::ApiError;

const DELETE_NEEDS_SELECT: &str = "; \"delete\" is only allowed if the role has \"select\" permission as \"where\" can't be used without \"select\" permission on the table";

#[derive(Debug, Clone)]
pub struct DeleteQuery {
    pub source: String,
    pub table: QualifiedTable,
    where_: Json,
    returning: Option<Vec<String>>,
    pub path: Path,
}

impl DeleteQuery {
    pub fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::new(v, path.clone(), "delete query")?;
        Ok(DeleteQuery {
            table: json::table_name(o.req("table")?, &o.at("table"))?,
            source: o
                .string("source")?
                .unwrap_or_else(|| super::lower::DEFAULT_SOURCE.to_owned()),
            where_: o.req("where")?.clone(),
            returning: o.strings("returning")?,
            path: path.clone(),
        })
    }
}

impl Lowerer<'_> {
    /// V2's `validateDeleteQWith`, in its order of checks.
    pub fn delete(&self, q: &DeleteQuery, session: &Session) -> Result<Mutation, ApiError> {
        let table = self.require_table(&q.table, &q.path)?;
        self.require_mutable(table, PermKind::Delete, &q.path)?;
        let access = self.mutation_access(table, session);
        let da = access
            .delete
            .as_ref()
            .ok_or_else(|| self.not_allowed(PermKind::Delete, table, &q.path))?;
        self.require_session_variables(da.filter.iter(), [], session, &q.path)?;
        let perm = self.select_permission_for(table, &q.path, DELETE_NEEDS_SELECT)?;
        let returning = match &q.returning {
            Some(cols) => {
                Some(self.returning(table, perm, cols, session, &q.path.key("returning"))?)
            }
            None => None,
        };
        let mut where_ = self.bool_exp(table, perm, &q.where_, &q.path.key("where"))?;
        nullify_missing_session_variables(&mut where_, session);
        Ok(Mutation::Delete(Box::new(Delete {
            table: mutation_table(table),
            filter: BoolExp::and(da.filter.clone(), Some(where_))
                .unwrap_or_else(BoolExp::always_true),
            output: mutation_output(returning),
        })))
    }
}

#[cfg(test)]
mod tests {
    use hge_core::Code;
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_errors() {
        let path = Path::root().key("args");
        let e = DeleteQuery::parse(&json!({"table": "t"}), &path).unwrap_err();
        assert_eq!(e.error.message, "key \"where\" not found");
        assert_eq!(e.error.path, "$.args");
        let e = DeleteQuery::parse(&json!([]), &path).unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing delete query failed, expected Object, but encountered Array"
        );
        assert_eq!(e.error.code, Code::ParseFailed);
    }
}
