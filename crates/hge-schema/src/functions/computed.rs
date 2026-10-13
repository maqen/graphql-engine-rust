//! Computed fields: a function of a table row, exposed as a field of the table's type.

use std::collections::HashSet;

use hge_core::{
    ColumnType, FunctionArg, InconsistentObject, QualifiedFunction, QualifiedTable, SourceCatalog,
    TableCatalog, TypeKind, Volatility,
};
use hge_metadata::TableEntry;
use serde::Deserialize;

use hge_ir::{FunctionArgValue, FunctionCall, Value};

use super::{ComputedFieldMeta, build_call, input_args, is_json, lookup, type_text};
use crate::tables::is_valid_name;

/// A resolved computed field.
#[derive(Debug, Clone)]
pub struct ComputedFieldInfo {
    pub name: String,
    pub function: QualifiedFunction,
    /// The field description (the metadata comment, else V2's default).
    pub description: String,
    /// The function's input arguments, in order.
    pub args: Vec<FunctionArg>,
    /// Index of the table-row argument in `args`.
    pub table_arg: usize,
    /// Index of the session argument in `args`.
    pub session_arg: Option<usize>,
    pub returns: ComputedReturn,
    /// Every column of the table, in order: the row passed to the function is rebuilt from
    /// them (`ROW(…)::table`).
    pub row_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComputedReturn {
    /// A scalar (or array) value.
    Scalar(ColumnType),
    /// Rows of a tracked table (`SETOF table` or a single `table` row, both a list in GraphQL).
    Table(QualifiedTable),
}

impl ComputedFieldInfo {
    /// The arguments the user passes (`args: {…}`), with their GraphQL names.
    pub fn input_args(&self) -> impl Iterator<Item = (usize, String, &FunctionArg)> + '_ {
        input_args(&self.args, move |i| {
            i == self.table_arg || Some(i) == self.session_arg
        })
    }

    /// The call on the current row of `table`; `inputs` holds the given input arguments by
    /// index into `args`.
    pub fn call(
        &self,
        table: &QualifiedTable,
        inputs: Vec<(usize, Value)>,
    ) -> Result<FunctionCall, &'static str> {
        let mut implicit = vec![(
            self.table_arg,
            FunctionArgValue::Row {
                table: table.clone(),
                columns: self.row_columns.clone(),
            },
        )];
        implicit.extend(self.session_arg.map(|i| (i, FunctionArgValue::Session)));
        build_call(&self.function, &self.args, implicit, inputs)
    }

    pub fn has_input_args(&self) -> bool {
        self.input_args().next().is_some()
    }

    /// `args` is required when some input argument has no default.
    pub fn args_required(&self) -> bool {
        self.input_args().any(|(_, _, a)| !a.has_default)
    }

    /// `<field>_<table>_args`
    pub fn args_type_name(&self, table_gql_name: &str) -> String {
        format!("{}_{table_gql_name}_args", self.name)
    }

    pub fn scalar_type(&self) -> Option<&ColumnType> {
        match &self.returns {
            ComputedReturn::Scalar(ty) => Some(ty),
            ComputedReturn::Table(_) => None,
        }
    }
}

/// V2's `in table "t": in computed field "f": <message>`.
fn scoped(table: &QualifiedTable, field: &str, message: &str) -> String {
    format!(
        "in table \"{}\": in computed field \"{field}\": {message}",
        table.v2_text()
    )
}

/// Resolve a table's computed fields. Unresolvable ones are reported and skipped.
pub fn resolve(
    source: &str,
    entry: &TableEntry,
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    tracked: &HashSet<QualifiedTable>,
    issues: &mut Vec<InconsistentObject>,
) -> Vec<ComputedFieldInfo> {
    let mut out = vec![];
    for raw in &entry.computed_fields {
        let issue = |name: &str, reason: String| {
            let mut def = raw.clone();
            if let Some(obj) = def.as_object_mut() {
                obj.entry("comment").or_insert(serde_json::Value::Null);
            }
            InconsistentObject::table_object(
                "computed_field",
                name,
                source,
                &entry.table,
                reason,
                def,
            )
        };
        let meta = match ComputedFieldMeta::deserialize(raw) {
            Ok(m) => m,
            Err(e) => {
                let name = raw.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                issues.push(issue(name, scoped(&entry.table, name, &e.to_string())));
                continue;
            }
        };
        match resolve_one(&meta, &entry.table, cat, catalog, tracked) {
            Ok(cf) => out.push(cf),
            Err(reason) => issues.push(issue(&meta.name, reason)),
        }
    }
    out
}

/// The error is the inconsistency reason.
fn resolve_one(
    meta: &ComputedFieldMeta,
    table: &QualifiedTable,
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    tracked: &HashSet<QualifiedTable>,
) -> Result<ComputedFieldInfo, String> {
    // V2 does not scope this one message.
    if cat.column(&meta.name).is_some() {
        return Err("field definition conflicts with postgres column".to_owned());
    }
    resolve_function(meta, table, cat, catalog, tracked).map_err(|m| scoped(table, &meta.name, &m))
}

fn resolve_function(
    meta: &ComputedFieldMeta,
    table: &QualifiedTable,
    cat: &TableCatalog,
    catalog: &SourceCatalog,
    tracked: &HashSet<QualifiedTable>,
) -> Result<ComputedFieldInfo, String> {
    let def = &meta.definition;
    let name = &meta.name;
    if !is_valid_name(name) {
        return Err(format!("\"{name}\" is not a valid GraphQL name"));
    }
    let f = lookup(catalog, &def.function)?;
    let fname = def.function.v2_text();
    let cannot = |why: String| {
        format!(
            "the computed field \"{name}\" cannot be added to table \"{}\" {why}",
            table.v2_text()
        )
    };
    if f.volatility == Volatility::Volatile {
        return Err(cannot(format!(
            "because the function \"{fname}\" is of type VOLATILE; cannot be added as a computed field"
        )));
    }

    let table_arg = match &def.table_argument {
        Some(arg) => f
            .args
            .iter()
            .position(|a| a.name.as_deref() == Some(arg))
            .ok_or_else(|| {
                cannot(format!(
                    "because \"{arg}\" is not an input argument of the function \"{fname}\""
                ))
            })?,
        None => 0,
    };
    let session_arg = match &def.session_argument {
        Some(arg) => {
            let i = f
                .args
                .iter()
                .position(|a| a.name.as_deref() == Some(arg))
                .ok_or_else(|| {
                    cannot(format!(
                        "because \"{arg}\" is not an input argument of the function \"{fname}\""
                    ))
                })?;
            if !is_json(&f.args[i]) {
                return Err(cannot(format!(
                    "because \"{arg}\" argument of the function \"{fname}\" is not of type JSON"
                )));
            }
            Some(i)
        }
        None => None,
    };

    let which = match &def.table_argument {
        Some(arg) => format!("\"{arg}\" argument"),
        None => "first argument".to_owned(),
    };
    let mut reasons = vec![];
    match f.args.get(table_arg) {
        None => reasons.push(format!(
            "the function \"{fname}\" has no input arguments of the table type"
        )),
        Some(a) => {
            if a.ty.kind != TypeKind::Composite || a.ty.dimensions > 0 {
                reasons.push(format!(
                    "{which} of the function \"{fname}\" is not COMPOSITE type"
                ));
            }
            if a.ty.schema != table.schema || a.ty.name != table.name || a.ty.dimensions > 0 {
                reasons.push(format!(
                    "{which} of the function \"{fname}\" of type \"{}\" is not the table to which the computed field is being added",
                    type_text(&a.ty)
                ));
            }
        }
    }
    if !reasons.is_empty() {
        let mut msg = cannot("for the following reasons:".to_owned());
        for r in reasons {
            msg.push_str("\n  • ");
            msg.push_str(&r);
        }
        msg.push('\n');
        return Err(msg);
    }

    let ret = &f.return_type;
    let returns = if ret.kind == TypeKind::Composite && ret.dimensions == 0 {
        let target = QualifiedTable::new(ret.schema.clone(), ret.name.clone());
        if !tracked.contains(&target) {
            return Err(cannot(format!(
                "because the function \"{fname}\" returning set of table \"{}\" is not tracked or not found in database",
                target.v2_text()
            )));
        }
        ComputedReturn::Table(target)
    } else if matches!(ret.kind, TypeKind::Base | TypeKind::Enum | TypeKind::Range) {
        ComputedReturn::Scalar(ret.clone())
    } else {
        return Err(cannot(format!(
            "because the function \"{fname}\" returning type {} is not a BASE type",
            ret.name
        )));
    };

    Ok(ComputedFieldInfo {
        name: name.clone(),
        function: def.function.clone(),
        description: meta
            .comment
            .clone()
            .unwrap_or_else(|| format!("A computed field, executes function \"{fname}\"")),
        args: f.args.clone(),
        table_arg,
        session_arg,
        returns,
        row_columns: cat.columns.keys().cloned().collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hge_core::{ColumnInfo, FunctionCatalog, TableKind, TableMutability};
    use indexmap::IndexMap;
    use serde_json::json;

    use super::*;

    fn ty(schema: &str, name: &str, kind: TypeKind) -> ColumnType {
        ColumnType {
            schema: schema.into(),
            name: name.into(),
            kind,
            dimensions: 0,
        }
    }

    fn arg(name: &str, ty: ColumnType) -> FunctionArg {
        FunctionArg {
            name: Some(name.into()),
            ty,
            has_default: false,
        }
    }

    fn function(name: &str, args: Vec<FunctionArg>, ret: ColumnType) -> FunctionCatalog {
        FunctionCatalog {
            name: QualifiedFunction::public(name),
            description: None,
            args,
            return_type: ret,
            returns_set: false,
            volatility: Volatility::Stable,
            variadic: false,
        }
    }

    fn setup() -> (TableCatalog, SourceCatalog) {
        let author = QualifiedTable::public("author");
        let row = ty("public", "author", TypeKind::Composite);
        let text = ty("pg_catalog", "text", TypeKind::Base);
        let int = ty("pg_catalog", "int4", TypeKind::Base);
        let json_ty = ty("pg_catalog", "json", TypeKind::Base);
        let cat = TableCatalog {
            name: author.clone(),
            kind: TableKind::Table,
            description: None,
            columns: IndexMap::from([(
                "id".to_owned(),
                ColumnInfo {
                    name: "id".into(),
                    ty: int.clone(),
                    nullable: false,
                    has_default: false,
                    is_identity: false,
                    is_generated: false,
                    is_identity_always: false,
                    description: None,
                },
            )]),
            primary_key: None,
            unique_constraints: vec![],
            constraints: vec![],
            mutability: TableMutability::ALL,
            foreign_keys: vec![],
        };
        let mut volatile = function("vol", vec![arg("a", row.clone())], text.clone());
        volatile.volatility = Volatility::Volatile;
        let fns = vec![
            function(
                "ok",
                vec![arg("s", json_ty), arg("a", row.clone())],
                text.clone(),
            ),
            volatile,
            function("wrong", vec![arg("x", int.clone())], text.clone()),
            function(
                "untracked",
                vec![arg("a", row.clone())],
                ty("public", "other", TypeKind::Composite),
            ),
            function("over", vec![arg("a", row.clone())], text.clone()),
            function(
                "unnamed",
                vec![
                    arg("a", row.clone()),
                    arg("x", int.clone()),
                    FunctionArg {
                        name: None,
                        ty: text.clone(),
                        has_default: false,
                    },
                ],
                text.clone(),
            ),
            function(
                "rec",
                vec![arg("a", row.clone())],
                ty("pg_catalog", "record", TypeKind::Pseudo),
            ),
            function("over", vec![arg("a", row)], int),
        ];
        let mut functions: HashMap<QualifiedFunction, Vec<FunctionCatalog>> = HashMap::new();
        for f in fns {
            functions.entry(f.name.clone()).or_default().push(f);
        }
        let catalog = SourceCatalog {
            tables: HashMap::from([(author, cat.clone())]),
            enums: HashMap::new(),
            functions,
            ..Default::default()
        };
        (cat, catalog)
    }

    fn resolve_one_def(def: serde_json::Value) -> Result<ComputedFieldInfo, String> {
        let (cat, catalog) = setup();
        let entry: TableEntry = serde_json::from_value(json!({
            "table": "author",
            "computed_fields": [def],
        }))
        .unwrap();
        let tracked = HashSet::from([QualifiedTable::public("author")]);
        let mut issues = vec![];
        let mut out = resolve("default", &entry, &cat, &catalog, &tracked, &mut issues);
        match issues.pop() {
            Some(i) => Err(i.reason),
            None => Ok(out.pop().unwrap()),
        }
    }

    #[test]
    fn resolves_table_and_session_arguments() {
        let cf = resolve_one_def(json!({"name": "c", "definition": {
            "function": "ok", "table_argument": "a", "session_argument": "s"
        }}))
        .unwrap();
        assert_eq!((cf.table_arg, cf.session_arg), (1, Some(0)));
        assert!(!cf.has_input_args());
        assert_eq!(cf.description, "A computed field, executes function \"ok\"");
    }

    #[test]
    fn numbers_unnamed_arguments_among_themselves() {
        let cf =
            resolve_one_def(json!({"name": "c", "definition": {"function": "unnamed"}})).unwrap();
        let names: Vec<String> = cf.input_args().map(|(_, n, _)| n).collect();
        assert_eq!(names, ["x", "arg_1"]);
        assert!(cf.args_required());
    }

    #[test]
    fn reports_v2_errors() {
        let prefix = "in table \"author\": in computed field \"c\": ";
        let cases = [
            (json!({"function": "nope"}), "no such function exists: \"nope\"".to_owned()),
            (
                json!({"function": "over"}),
                "function \"over\" is overloaded. Overloaded functions are not supported".to_owned(),
            ),
            (
                json!({"function": "vol"}),
                "the computed field \"c\" cannot be added to table \"author\" because the function \"vol\" is of type VOLATILE; cannot be added as a computed field".to_owned(),
            ),
            (
                json!({"function": "wrong"}),
                "the computed field \"c\" cannot be added to table \"author\" for the following reasons:\n  • first argument of the function \"wrong\" is not COMPOSITE type\n  • first argument of the function \"wrong\" of type \"pg_catalog.int4\" is not the table to which the computed field is being added\n".to_owned(),
            ),
            (
                json!({"function": "ok", "table_argument": "zz"}),
                "the computed field \"c\" cannot be added to table \"author\" because \"zz\" is not an input argument of the function \"ok\"".to_owned(),
            ),
            (
                json!({"function": "ok", "table_argument": "a", "session_argument": "a"}),
                "the computed field \"c\" cannot be added to table \"author\" because \"a\" argument of the function \"ok\" is not of type JSON".to_owned(),
            ),
            (
                json!({"function": "rec"}),
                "the computed field \"c\" cannot be added to table \"author\" because the function \"rec\" returning type record is not a BASE type".to_owned(),
            ),
            (
                json!({"function": "untracked"}),
                "the computed field \"c\" cannot be added to table \"author\" because the function \"untracked\" returning set of table \"other\" is not tracked or not found in database".to_owned(),
            ),
        ];
        for (def, reason) in cases {
            let err = resolve_one_def(json!({"name": "c", "definition": def})).unwrap_err();
            assert_eq!(err, format!("{prefix}{reason}"));
        }
        let err =
            resolve_one_def(json!({"name": "id", "definition": {"function": "ok"}})).unwrap_err();
        assert_eq!(err, "field definition conflicts with postgres column");
    }
}
