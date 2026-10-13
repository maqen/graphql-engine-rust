//! Legacy `update`: `{"table", "where", "$set"?, "$inc"?, "$mul"?, "$default"?, "returning"?}`
//! → `{"affected_rows": n, "returning"?: [...]}` (V2's `Hasura.RQL.DML.Update`).

use hge_core::{Code, QualifiedTable, Session};
use hge_ir::{BoolExp, Mutation, Update, UpdateExp};
use hge_schema::TableInfo;
use hge_schema::mutation::UpdateAccess;
use hge_schema::tables::ColumnInfo;
use serde_json::Value as Json;

use super::boolexp::nullify_missing_session_variables;
use super::json::{self, Obj, Path, err400};
use super::lower::{Lowerer, PermKind, column_info, dml_value, mutation_output, mutation_table};
use hge_metadata_api::ApiError;

const UPDATE_NEEDS_SELECT: &str = "; \"update\" is only allowed if the role has \"select\" permission as \"where\" can't be used without \"select\" permission on the table";

#[derive(Debug, Clone)]
pub struct UpdateQuery {
    pub source: String,
    pub table: QualifiedTable,
    where_: Json,
    set: Vec<(String, Json)>,
    inc: Vec<(String, Json)>,
    mul: Vec<(String, Json)>,
    default: Vec<String>,
    returning: Option<Vec<String>>,
    pub path: Path,
}

/// The operator objects, in V2's order of evaluation.
#[derive(Debug, Clone, Copy)]
enum Op {
    Set,
    Inc,
    Mul,
}

impl UpdateQuery {
    pub fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::new(v, path.clone(), "update query")?;
        let table = json::table_name(o.req("table")?, &o.at("table"))?;
        let source = o
            .string("source")?
            .unwrap_or_else(|| super::lower::DEFAULT_SOURCE.to_owned());
        let where_ = o.req("where")?.clone();
        let owned = |items: Vec<(&str, &Json)>| -> Vec<(String, Json)> {
            items
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.clone()))
                .collect()
        };
        let operator = |key: &str| -> Result<Vec<(String, Json)>, ApiError> {
            match o.opt(key) {
                Some(v) => Ok(owned(json::column_values(v, &o.at(key))?)),
                None => Ok(vec![]),
            }
        };
        // V2: `o .: "$set" <|> o .:? "values"`; a `$set` that is not an object falls back to
        // the old `values` key (and is otherwise ignored).
        let set = match o.get("$set") {
            Some(set @ Json::Object(_)) => owned(json::column_values(set, &o.at("$set"))?),
            _ => operator("values")?,
        };
        Ok(UpdateQuery {
            table,
            source,
            where_,
            set,
            inc: operator("$inc")?,
            mul: operator("$mul")?,
            default: o.strings("$default")?.unwrap_or_default(),
            returning: o.strings("returning")?,
            path: path.clone(),
        })
    }
}

impl Lowerer<'_> {
    /// V2's `validateUpdateQueryWith`, in its order of checks.
    pub fn update(&self, q: &UpdateQuery, session: &Session) -> Result<Mutation, ApiError> {
        let table = self.require_table(&q.table, &q.path.key("table"))?;
        self.require_mutable(table, PermKind::Update, &q.path)?;
        let access = self.mutation_access(table, session);
        let ua = access
            .update
            .as_ref()
            .ok_or_else(|| self.not_allowed(PermKind::Update, table, &q.path))?;
        self.require_session_variables(
            ua.filter.iter().chain(ua.check.iter()),
            ua.presets.iter().map(|(_, v)| v),
            session,
            &q.path,
        )?;
        let perm = self.select_permission_for(table, &q.path, UPDATE_NEEDS_SELECT)?;

        let mut items: Vec<(String, UpdateExp)> = vec![];
        for (op, key, values) in [
            (Op::Set, "$set", &q.set),
            (Op::Inc, "$inc", &q.inc),
            (Op::Mul, "$mul", &q.mul),
        ] {
            let op_path = q.path.key(key);
            for (c, v) in values {
                let info = self.update_column(table, ua, c, &op_path)?;
                let value = dml_value(v, &info.ty, &op_path)?;
                items.push((
                    c.clone(),
                    match op {
                        Op::Set => UpdateExp::Set(value),
                        Op::Inc => UpdateExp::Inc(value),
                        Op::Mul => UpdateExp::Mul(value),
                    },
                ));
            }
        }
        let default_path = q.path.key("$default");
        for c in &q.default {
            self.update_column(table, ua, c, &default_path)?;
            items.push((c.clone(), UpdateExp::Default));
        }

        let returning = match &q.returning {
            Some(cols) => Some(self.returning(table, perm, cols, &q.path.key("returning"))?),
            None => None,
        };

        let mut set: Vec<(String, UpdateExp)> = ua
            .presets
            .iter()
            .map(|(c, v)| (c.clone(), UpdateExp::Set(v.clone())))
            .collect();
        // V2 builds a map from the items, so a column given twice keeps its last value.
        for (c, exp) in items {
            set.retain(|(existing, _)| existing != &c);
            set.push((c, exp));
        }
        if set.is_empty() {
            return Err(err400(
                Code::UnexpectedPayload,
                "atleast one of $set, $inc, $mul has to be present",
                &q.path,
            ));
        }

        let mut where_ = self.bool_exp(table, perm, &q.where_, &q.path.key("where"))?;
        nullify_missing_session_variables(&mut where_, session);
        Ok(Mutation::Update(Box::new(Update {
            table: mutation_table(table),
            set,
            filter: BoolExp::and(ua.filter.clone(), Some(where_))
                .unwrap_or_else(BoolExp::always_true),
            check: ua.check.clone().filter(|c| !c.is_true()),
            output: mutation_output(returning),
        })))
    }

    /// V2's `convOp` checks on one column: not preset, updatable by the role, a column.
    fn update_column<'t>(
        &self,
        table: &'t TableInfo,
        ua: &UpdateAccess,
        column: &str,
        path: &Path,
    ) -> Result<&'t ColumnInfo, ApiError> {
        if ua.presets.iter().any(|(p, _)| p == column) {
            return Err(err400(
                Code::NotSupported,
                format!(
                    "column \"{column}\" is not updatable for role \"{}\"; its value is predefined in permission",
                    self.role
                ),
                path,
            ));
        }
        self.check_column(PermKind::Update, &ua.columns, column, path)?;
        column_info(table, column, "relationships can't be updated", path)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(args: Json) -> Result<UpdateQuery, ApiError> {
        UpdateQuery::parse(&args, &Path::root().key("args"))
    }

    #[test]
    fn where_is_required() {
        let e = parse(json!({"table": "person", "$set": {"details": null}})).unwrap_err();
        assert_eq!(e.error.message, "key \"where\" not found");
        assert_eq!(e.error.path, "$.args");
        assert_eq!(e.error.code, Code::ParseFailed);
    }

    #[test]
    fn operators_must_be_objects() {
        let e = parse(json!({"table": "t", "where": {}, "$inc": [1]})).unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing HashMap ~Text failed, expected Object, but encountered Array"
        );
        assert_eq!(e.error.path, r#"$.args["$inc"]"#);
        let e = parse(json!({"table": "t", "where": {}, "$default": "c"})).unwrap_err();
        assert_eq!(e.error.path, r#"$.args["$default"]"#);
    }

    #[test]
    fn set_falls_back_to_values() {
        let q = parse(json!({"table": "t", "where": {}, "values": {"a": 1}})).unwrap();
        assert_eq!(q.set, vec![("a".to_owned(), json!(1))]);
        // A `$set` that is not an object is ignored, as in V2.
        let q = parse(json!({"table": "t", "where": {}, "$set": 3})).unwrap();
        assert!(q.set.is_empty());
    }
}
