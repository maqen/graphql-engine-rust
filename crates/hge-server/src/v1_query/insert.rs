//! Legacy `insert`: `{"table", "objects": [{col: value}], "on_conflict"?, "returning"?}` →
//! `{"affected_rows": n, "returning"?: [...]}` (V2's `Hasura.RQL.DML.Insert`).

use hge_core::{Code, QualifiedTable, Session};
use hge_ir::{
    BoolExp, ConflictTarget, Insert, InsertCheck, InsertInto, InsertObject, Mutation, OnConflict,
};
use hge_schema::TableInfo;
use hge_schema::mutation::UpdateAccess;
use serde_json::Value as Json;

use super::json::{self, Obj, Path, display_table, err400, parse_failed};
use super::lower::{Lowerer, PermKind, column_info, dml_value, mutation_output, mutation_table};
use hge_metadata_api::ApiError;

const ON_CONFLICT_RECORD: &str = "Hasura.RQL.DML.Types.OnConflict(OnConflict)";

pub const RETURNING_NEEDS_SELECT: &str =
    "; \"returning\" can only be used if the role has \"select\" permission on the table";

#[derive(Debug, Clone)]
pub struct InsertQuery {
    pub source: String,
    pub table: QualifiedTable,
    /// Decoded while lowering, after the other arguments, as in V2.
    objects: Json,
    on_conflict: Option<OnConflictArg>,
    returning: Option<Vec<String>>,
    pub path: Path,
}

#[derive(Debug, Clone)]
struct OnConflictArg {
    constraint_on: Option<Vec<String>>,
    constraint: Option<String>,
    action: ConflictAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictAction {
    Ignore,
    Update,
}

impl InsertQuery {
    pub fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::new(v, path.clone(), "insert query")?;
        let table = json::table_name(o.req("table")?, &o.at("table"))?;
        let source = o
            .string("source")?
            .unwrap_or_else(|| super::lower::DEFAULT_SOURCE.to_owned());
        let objects = o.req("objects")?.clone();
        let on_conflict = o
            .opt("on_conflict")
            .map(|v| OnConflictArg::parse(v, &o.at("on_conflict")))
            .transpose()?;
        Ok(InsertQuery {
            table,
            source,
            objects,
            on_conflict,
            returning: o.strings("returning")?,
            path: path.clone(),
        })
    }
}

impl OnConflictArg {
    fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::record(v, path.clone(), ON_CONFLICT_RECORD)?;
        let constraint_on = match o.opt("constraint_on") {
            None => None,
            Some(Json::String(c)) => Some(vec![c.clone()]),
            Some(cols @ Json::Array(_)) => Some(json::strings(cols, &o.at("constraint_on"))?),
            Some(_) => {
                return Err(parse_failed(
                    "Expecting String or Array",
                    &o.at("constraint_on"),
                ));
            }
        };
        let constraint = o.string("constraint")?;
        let action = match o.req("action")? {
            Json::String(s) if s == "ignore" => ConflictAction::Ignore,
            Json::String(s) if s == "update" => ConflictAction::Update,
            _ => {
                return Err(parse_failed(
                    "Expecting 'ignore' or 'update'",
                    &o.at("action"),
                ));
            }
        };
        Ok(OnConflictArg {
            constraint_on,
            constraint,
            action,
        })
    }
}

impl Lowerer<'_> {
    /// V2's `convInsertQuery`, in its order of checks.
    pub fn insert(&self, q: &InsertQuery, session: &Session) -> Result<Mutation, ApiError> {
        let objects_path = q.path.key("objects");
        let objects = match &q.objects {
            Json::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, o)| json::column_values(o, &objects_path.index(i)))
                .collect::<Result<Vec<_>, _>>()?,
            other => return Err(json::type_mismatch("[]", "Array", other, &objects_path)),
        };
        if objects.is_empty() {
            return Err(err400(
                Code::UnexpectedPayload,
                "objects should not be empty",
                &objects_path,
            ));
        }

        let table = self.require_table(&q.table, &q.path)?;
        self.require_mutable(table, PermKind::Insert, &q.path)?;
        let access = self.mutation_access(table, session);
        let ia = access
            .insert
            .as_ref()
            .ok_or_else(|| self.not_allowed(PermKind::Insert, table, &q.path))?;
        self.require_session_variables(
            ia.check.iter(),
            ia.presets.iter().map(|(_, v)| v),
            session,
            &q.path,
        )?;

        let returning = match &q.returning {
            Some(cols) => {
                let perm = self.select_permission_for(table, &q.path, RETURNING_NEEDS_SELECT)?;
                Some(self.returning(table, perm, cols, session, &q.path.key("returning"))?)
            }
            None => None,
        };

        // Columns given in any object, in first-seen order (an upsert updates these).
        let mut input_columns: Vec<String> = vec![];
        let mut rows = Vec::with_capacity(objects.len());
        for (i, obj) in objects.iter().enumerate() {
            let obj_path = objects_path.index(i);
            let mut columns = Vec::with_capacity(obj.len() + ia.presets.len());
            for (c, v) in obj {
                let info = column_info(table, c, "relationships can't be inserted", &obj_path)?;
                if ia.presets.iter().any(|(p, _)| p == c) {
                    return Err(err400(
                        Code::NotSupported,
                        format!(
                            "column \"{c}\" is not insertable for role \"{}\"",
                            self.role
                        ),
                        &obj_path,
                    ));
                }
                // V2's legacy insert does not check the permission's columns at all; we do,
                // with its message for the other operations.
                self.check_column(PermKind::Insert, &ia.columns, c, &obj_path)?;
                columns.push(((*c).to_owned(), dml_value(v, &info.ty, &obj_path.key(c))?));
                if !input_columns.iter().any(|ic| ic == c) {
                    input_columns.push((*c).to_owned());
                }
            }
            columns.extend(ia.presets.iter().cloned());
            rows.push(InsertObject {
                columns,
                before: vec![],
                after: vec![],
                path: obj_path.to_string(),
            });
        }

        let on_conflict = match &q.on_conflict {
            None => None,
            Some(oc) => {
                let oc_path = q.path.key("on_conflict");
                let Some(ua) = access.update.as_ref() else {
                    return Err(err400(
                        Code::PermissionDenied,
                        format!(
                            "upsert is not allowed for role \"{}\" since update permissions are not defined",
                            self.role
                        ),
                        &oc_path,
                    ));
                };
                Some(conflict_clause(table, oc, ua, &input_columns, &oc_path)?)
            }
        };

        // V2 checks updated rows against the update permission's check when there is one,
        // else against the insert check.
        let update_check = on_conflict
            .as_ref()
            .and(access.update.as_ref())
            .and_then(|u| u.check.clone());
        // An upsert also requires the update permission's session variables. V2 reads them
        // with `current_setting`, so a missing one is NULL and a NULL check passes; we refuse
        // instead, as plain `update` does.
        if on_conflict.is_some()
            && let Some(ua) = access.update.as_ref()
        {
            self.require_session_variables(
                ua.filter.iter().chain(ua.check.iter()),
                ua.presets.iter().map(|(_, v)| v),
                session,
                &q.path,
            )?;
        }
        let check = match (&ia.check, update_check) {
            (None, None) => None,
            (insert, update) => Some(InsertCheck {
                insert: insert.clone().unwrap_or_else(BoolExp::always_true),
                update,
            }),
        };
        Ok(Mutation::Insert(Box::new(Insert {
            into: InsertInto {
                table: mutation_table(table),
                objects: rows,
                on_conflict,
                check,
                path: q.path.to_string(),
            },
            output: mutation_output(returning),
        })))
    }
}

/// V2's `buildConflictClause`.
fn conflict_clause(
    table: &TableInfo,
    oc: &OnConflictArg,
    ua: &UpdateAccess,
    input_columns: &[String],
    path: &Path,
) -> Result<OnConflict, ApiError> {
    let validate_columns = |cols: &[String]| -> Result<(), ApiError> {
        let cols_path = path.key("constraint_on");
        if cols.is_empty() {
            return Err(err400(
                Code::ParseFailed,
                "expected a non-empty array of column names",
                &cols_path,
            ));
        }
        for (i, c) in cols.iter().enumerate() {
            column_info(table, c, "", &cols_path.index(i))?;
        }
        Ok(())
    };
    let validate_constraint = |name: &str| -> Result<(), ApiError> {
        if table.constraints.iter().any(|c| c.name == name) {
            return Ok(());
        }
        Err(err400(
            Code::Unexpected,
            format!(
                "constraint \"{name}\" for table \"{}\" does not exist",
                display_table(&table.name)
            ),
            &path.key("constraint"),
        ))
    };
    // V2's `validateInpCols`: an upsert updates every input column.
    let validate_update = || -> Result<(), ApiError> {
        for c in input_columns {
            if !ua.columns.contains(c) {
                return Err(err400(
                    Code::ValidationFailed,
                    format!("column \"{c}\" is not updatable"),
                    path,
                ));
            }
        }
        Ok(())
    };
    let (constraint, target) = match (&oc.constraint_on, &oc.constraint) {
        (Some(_), Some(_)) => {
            return Err(err400(
                Code::UnexpectedPayload,
                "'constraint' and 'constraint_on' cannot be set at a time",
                path,
            ));
        }
        (None, None) if oc.action == ConflictAction::Update => {
            return Err(err400(
                Code::UnexpectedPayload,
                "Expecting 'constraint' or 'constraint_on' when the 'action' is 'update'",
                path,
            ));
        }
        (None, None) => (String::new(), Some(ConflictTarget::Any)),
        (Some(cols), None) => {
            validate_columns(cols)?;
            (String::new(), Some(ConflictTarget::Columns(cols.clone())))
        }
        (None, Some(name)) => {
            validate_constraint(name)?;
            (name.clone(), None)
        }
    };
    Ok(match oc.action {
        ConflictAction::Ignore => OnConflict {
            constraint,
            target,
            update_columns: vec![],
            presets: vec![],
            filter: None,
        },
        ConflictAction::Update => {
            validate_update()?;
            OnConflict {
                constraint,
                target,
                // With no input columns, V2 still updates the update presets; an empty
                // `update_columns` would mean `DO NOTHING`.
                update_columns: if input_columns.is_empty() {
                    ua.presets.iter().map(|(c, _)| c.clone()).collect()
                } else {
                    input_columns.to_vec()
                },
                presets: ua.presets.clone(),
                filter: ua.filter.clone(),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(args: Json) -> Result<InsertQuery, ApiError> {
        InsertQuery::parse(&args, &Path::root().key("args"))
    }

    #[test]
    fn requires_table_and_objects() {
        let e = parse(json!({"objects": []})).unwrap_err();
        assert_eq!(e.error.message, "key \"table\" not found");
        assert_eq!(e.error.path, "$.args");
        let e = parse(json!({"table": "author"})).unwrap_err();
        assert_eq!(e.error.message, "key \"objects\" not found");
        assert_eq!(e.error.code, Code::ParseFailed);
    }

    #[test]
    fn on_conflict_errors() {
        let e = parse(json!({
            "table": "a", "objects": [{}], "on_conflict": {"constraint_on": ["id"]}
        }))
        .unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing Hasura.RQL.DML.Types.OnConflict(OnConflict) failed, key \"action\" not found"
        );
        assert_eq!(e.error.path, "$.args.on_conflict");
        let e = parse(json!({
            "table": "a", "objects": [{}], "on_conflict": {"action": "random"}
        }))
        .unwrap_err();
        assert_eq!(e.error.message, "Expecting 'ignore' or 'update'");
        assert_eq!(e.error.path, "$.args.on_conflict.action");
        let e = parse(json!({
            "table": "a", "objects": [{}], "on_conflict": {"constraint_on": 1, "action": "ignore"}
        }))
        .unwrap_err();
        assert_eq!(e.error.message, "Expecting String or Array");
        assert_eq!(e.error.path, "$.args.on_conflict.constraint_on");
    }

    #[test]
    fn constraint_on_accepts_a_string() {
        let q = parse(json!({
            "table": "a", "objects": [{}],
            "on_conflict": {"constraint_on": "id", "action": "update"}
        }))
        .unwrap();
        assert_eq!(
            q.on_conflict.unwrap().constraint_on,
            Some(vec!["id".to_owned()])
        );
    }

    #[test]
    fn returning_must_be_column_names() {
        let e = parse(json!({"table": "a", "objects": [{}], "returning": ["id", 1]})).unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing Text failed, expected String, but encountered Number"
        );
        assert_eq!(e.error.path, "$.args.returning[1]");
    }
}
