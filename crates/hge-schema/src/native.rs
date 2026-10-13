//! Logical models and native queries.
//!
//! A logical model becomes a [`TableInfo`] without a database table (its fields are its
//! columns), so the object, `bool_exp`, `order_by` and column enum types, permissions and
//! lowering of tables apply to it unchanged. A native query is a root field selecting rows of
//! its logical model from its SQL code instead of a table.
//!
//! Native queries are validated as V2 does: the backend prepares (never executes)
//! `WITH _cte_logimo_vali_ AS (<code>) SELECT <fields> FROM _cte_logimo_vali_`, see
//! [`requests`]; [`resolve`] turns the outcome into inconsistencies with V2's messages.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use hge_core::{
    ColumnType, InconsistentObject, NativeQueryCheck, QualifiedTable, SourceCatalog,
    TableMutability,
};
use hge_metadata::{Permission, SelectPermission, Source, TableEntry};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::tables::{ColumnInfo, RootFieldNames, TableInfo, Tables};

/// `logical_models[]` of a source.
#[derive(Debug, Clone, Deserialize)]
struct LogicalModelMeta {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    fields: Vec<LogicalModelField>,
    #[serde(default)]
    select_permissions: Vec<Permission<SelectPermission>>,
}

#[derive(Debug, Clone, Deserialize)]
struct LogicalModelField {
    name: String,
    #[serde(rename = "type")]
    ty: Value,
    #[serde(default)]
    description: Option<String>,
}

impl LogicalModelField {
    /// `(scalar type name, nullable)`; `None` for nested logical models and arrays, which
    /// are not supported yet.
    fn scalar(&self) -> Option<(&str, bool)> {
        let scalar = self.ty.get("scalar")?.as_str()?;
        let nullable = self.ty.get("nullable").and_then(Value::as_bool);
        Some((scalar, nullable.unwrap_or(false)))
    }
}

/// `native_queries[]` of a source.
#[derive(Debug, Clone, Deserialize)]
struct NativeQueryMeta {
    root_field_name: String,
    code: String,
    #[serde(default)]
    arguments: IndexMap<String, ArgumentMeta>,
    returns: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ArgumentMeta {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    nullable: bool,
    #[serde(default)]
    description: Option<String>,
}

/// A piece of a native query's code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodePart {
    Sql(String),
    /// `{{name}}`
    Arg(String),
}

/// A native query argument.
#[derive(Debug, Clone)]
pub struct NativeQueryArg {
    pub ty: ColumnType,
    pub nullable: bool,
    pub description: Option<String>,
}

/// A resolved native query.
#[derive(Debug, Clone)]
pub struct NativeQueryInfo {
    pub source: Arc<str>,
    pub root_field: String,
    pub description: Option<String>,
    pub arguments: IndexMap<String, NativeQueryArg>,
    pub code: Vec<CodePart>,
    /// The logical model's pseudo-table name (see [`logical_model_name`]).
    pub returns: QualifiedTable,
    /// The metadata as V2 reports it in inconsistencies.
    pub definition: Value,
}

impl NativeQueryInfo {
    /// The input type of the `args` argument.
    pub fn arguments_type_name(&self) -> String {
        format!("{}_arguments", self.root_field)
    }

    /// V2's description of the `args` argument and of its type (sic, no space).
    pub fn arguments_description(&self) -> String {
        format!("{}Native Query Arguments", self.root_field)
    }
}

/// The name logical models are registered under in [`Tables`]: an empty schema, which no
/// database table has.
pub fn logical_model_name(name: &str) -> QualifiedTable {
    QualifiedTable::new(String::new(), name.to_owned())
}

fn logical_models(source: &Source) -> Vec<LogicalModelMeta> {
    parse_list(source, "logical_models")
}

fn native_queries(source: &Source) -> Vec<NativeQueryMeta> {
    parse_list(source, "native_queries")
}

fn parse_list<T: for<'de> Deserialize<'de>>(source: &Source, key: &str) -> Vec<T> {
    source
        .extra
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| T::deserialize(v).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Roles named by logical model select permissions (they exist like table permission roles).
pub fn permission_roles(source: &Source) -> impl Iterator<Item = &str> {
    source
        .extra
        .get("logical_models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("select_permissions")?.as_array())
        .flatten()
        .filter_map(|p| p.get("role")?.as_str())
}

/// Drop native queries whose root field or types clash with those of tables, tracked
/// functions, or earlier native queries (V2 rejects the schema; we report the native query
/// as inconsistent). Call after tracked functions are resolved.
pub(crate) fn drop_conflicting(tables: &mut Tables, issues: &mut Vec<InconsistentObject>) {
    let queries = std::mem::take(&mut tables.native_queries);
    let names = crate::build::table_schema_names(tables);
    let mut fields: HashSet<String> = HashSet::new();
    // Logical model types are shared by the native queries returning the same model.
    let mut types: HashSet<String> = HashSet::new();
    let mut kept = Vec::with_capacity(queries.len());
    for nq in queries {
        let model = tables.logical_model(&nq.source, &nq.returns).cloned();
        let field = &nq.root_field;
        let mut new_types = vec![];
        if !nq.arguments.is_empty() {
            new_types.push(nq.arguments_type_name());
        }
        if let Some(m) = &model {
            for t in [
                m.type_name().to_owned(),
                m.bool_exp_name(),
                m.order_by_name(),
                m.select_column_name(),
            ] {
                if !types.contains(&t) {
                    new_types.push(t);
                }
            }
        }
        let reason = if names.query_fields.contains(field)
            || names.subscription_fields.contains(field)
            || fields.contains(field)
        {
            Some(format!(
                "Encountered conflicting definitions in the selection set for 'subscription_root' \
                 for field '{field}'. Fields must not be defined more than once across all sources."
            ))
        } else {
            new_types
                .iter()
                .find(|t| names.types.contains_key(*t) || types.contains(*t))
                .map(|t| format!("Found conflicting definitions for GraphQL type '{t}'."))
        };
        match reason {
            Some(reason) => issues.push(InconsistentObject::new(
                "native_query",
                format!("native_query {field} in source {}", nq.source),
                reason,
                nq.definition.clone(),
            )),
            None => {
                fields.insert(field.clone());
                types.extend(new_types);
                kept.push(nq);
            }
        }
    }
    tables.native_queries = kept;
}

/// Split code at `{{name}}` references (names are trimmed), without trailing semicolons.
pub fn parse_code(code: &str) -> Vec<CodePart> {
    let mut parts = vec![];
    let mut rest = code;
    let mut sql = String::new();
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        sql.push_str(&rest[..start]);
        if !sql.is_empty() {
            parts.push(CodePart::Sql(std::mem::take(&mut sql)));
        }
        parts.push(CodePart::Arg(
            rest[start + 2..start + 2 + len].trim().to_owned(),
        ));
        rest = &rest[start + 2 + len + 2..];
    }
    sql.push_str(rest);
    if !sql.is_empty() {
        parts.push(CodePart::Sql(sql));
    }
    // V2's `trimQueryEnd`: trailing whitespace and semicolons would end the wrapping query.
    if let Some(CodePart::Sql(last)) = parts.last_mut() {
        let len = last
            .trim_end_matches(|c: char| c.is_whitespace() || c == ';')
            .len();
        last.truncate(len);
        if last.is_empty() {
            parts.pop();
        }
    }
    parts
}

/// The arguments the code uses, in order of first use (V2 numbers the parameters so).
fn used_arguments(code: &[CodePart]) -> Vec<&str> {
    let mut out: Vec<&str> = vec![];
    for part in code {
        if let CodePart::Arg(a) = part
            && !out.contains(&a.as_str())
        {
            out.push(a);
        }
    }
    out
}

/// The code with `$1`, `$2`, … for its arguments, numbered as in [`used_arguments`].
fn numbered_code(code: &[CodePart]) -> String {
    let used = used_arguments(code);
    let mut out = String::new();
    for part in code {
        match part {
            CodePart::Sql(s) => out.push_str(s),
            CodePart::Arg(a) => {
                let n = used.iter().position(|u| u == a).expect("listed") + 1;
                out.push_str(&format!("${n}"));
            }
        }
    }
    out
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// V2's validation statement body (`WITH _cte_logimo_vali_ AS (…) SELECT … FROM …`).
fn validation_sql(code: &[CodePart], model: &LogicalModelMeta) -> String {
    let columns: Vec<String> = model
        .fields
        .iter()
        .filter(|f| f.scalar().is_some())
        .map(|f| quote_ident(&f.name))
        .collect();
    format!(
        "WITH _cte_logimo_vali_ AS (\n{}\n\n)\nSELECT {}\nFROM _cte_logimo_vali_",
        numbered_code(code),
        columns.join(", ")
    )
}

/// The native query's validation check, if it gets that far: its logical model exists and
/// every argument the code uses is declared.
fn check_for(nq: &NativeQueryMeta, models: &[LogicalModelMeta]) -> Option<NativeQueryCheck> {
    let model = models.iter().find(|m| m.name == nq.returns)?;
    let code = parse_code(&nq.code);
    let used = used_arguments(&code);
    let param_types = used
        .iter()
        .map(|a| nq.arguments.get(*a).map(|arg| arg.ty.clone()))
        .collect::<Option<Vec<_>>>()?;
    Some(NativeQueryCheck {
        sql: validation_sql(&code, model),
        param_types,
    })
}

/// What the backend must resolve for a source: the scalar type names of logical model fields
/// and native query arguments, and the native queries to validate.
pub fn requests(source: &Source) -> (Vec<String>, Vec<NativeQueryCheck>) {
    let models = logical_models(source);
    let queries = native_queries(source);
    let mut types = BTreeSet::new();
    for m in &models {
        for f in &m.fields {
            if let Some((scalar, _)) = f.scalar() {
                types.insert(scalar.to_owned());
            }
        }
    }
    for q in &queries {
        types.extend(q.arguments.values().map(|a| a.ty.clone()));
    }
    let checks = queries
        .iter()
        .filter_map(|q| check_for(q, &models))
        .collect();
    (types.into_iter().collect(), checks)
}

/// Resolve the logical models (into `tables.logical_models`) and native queries (into
/// `tables.native_queries`) of a source; invalid native queries are reported and skipped.
pub fn resolve(
    source: &Source,
    catalog: &SourceCatalog,
    tables: &mut Tables,
    issues: &mut Vec<InconsistentObject>,
) {
    let source_name: Arc<str> = Arc::from(source.name.as_str());
    let models = logical_models(source);
    let scalar_type = |name: &str| {
        catalog
            .types
            .get(name)
            .map(|t| t.ty.clone())
            .unwrap_or_else(|| ColumnType::builtin(name))
    };
    for m in &models {
        let info = Arc::new(logical_model_table(&source_name, m, &scalar_type));
        tables
            .by_name
            .insert((source_name.clone(), info.name.clone()), info.clone());
        tables.logical_models.push(info);
    }

    for nq in native_queries(source) {
        match resolve_native_query(&source_name, &nq, &models, catalog) {
            Ok(info) => tables.native_queries.push(Arc::new(info)),
            Err(message) => issues.push(native_query_issue(&source.name, &nq, catalog, message)),
        }
    }
}

fn logical_model_table(
    source: &Arc<str>,
    m: &LogicalModelMeta,
    scalar_type: &impl Fn(&str) -> ColumnType,
) -> TableInfo {
    let mut columns = IndexMap::new();
    for f in &m.fields {
        let Some((scalar, nullable)) = f.scalar() else {
            continue;
        };
        columns.insert(
            f.name.clone(),
            ColumnInfo {
                name: f.name.clone(),
                gql_name: f.name.clone(),
                ty: scalar_type(scalar),
                nullable,
                description: f.description.clone(),
                mutable: false,
                enum_ref: None,
            },
        );
    }
    let name = logical_model_name(&m.name);
    let mut entry = TableEntry::new(name.clone());
    entry.select_permissions = m.select_permissions.clone();
    TableInfo {
        source: source.clone(),
        name,
        gql_name: m.name.clone(),
        description: m.description.clone(),
        columns,
        primary_key: None,
        constraints: vec![],
        mutability: TableMutability {
            insertable: false,
            updatable: false,
            deletable: false,
        },
        object_relationships: vec![],
        array_relationships: vec![],
        computed_fields: vec![],
        root_fields: RootFieldNames {
            select: m.name.clone(),
            select_by_pk: format!("{}_by_pk", m.name),
            select_aggregate: format!("{}_aggregate", m.name),
            select_comment: None,
            select_by_pk_comment: None,
            select_aggregate_comment: None,
        },
        is_enum: false,
        enum_values: None,
        logical_model: true,
        entry,
        permissions: Default::default(),
    }
}

/// Why validation failed: V2's `message`, a string or an object.
enum Failure {
    /// Reported as the inconsistency's reason.
    Reason(String),
    /// "Failed to validate query", with this message.
    Validation(Value),
}

fn resolve_native_query(
    source: &Arc<str>,
    nq: &NativeQueryMeta,
    models: &[LogicalModelMeta],
    catalog: &SourceCatalog,
) -> Result<NativeQueryInfo, Failure> {
    let Some(model) = models.iter().find(|m| m.name == nq.returns) else {
        return Err(Failure::Reason(format!(
            "The logical model {} could not be found",
            nq.returns
        )));
    };
    let code = parse_code(&nq.code);
    let undeclared: Vec<String> = used_arguments(&code)
        .into_iter()
        .filter(|a| !nq.arguments.contains_key(*a))
        .map(|a| format!("'{a}'"))
        .collect();
    if !undeclared.is_empty() {
        return Err(Failure::Reason(format!(
            "The following columns are used in the query, but are not declared as arguments: [{}]",
            undeclared.join(",")
        )));
    }
    let check = check_for(nq, models).expect("model and arguments checked");
    // Without a result (a catalog given rather than introspected) the query is trusted.
    match catalog.native_queries.get(&check) {
        Some(Err(error)) => {
            return Err(Failure::Validation(json!({
                "arguments": [],
                "error": error,
                "prepared": false,
                "statement": prepare_statement(&check, catalog),
            })));
        }
        Some(Ok(columns)) => {
            for f in &model.fields {
                let Some((scalar, _)) = f.scalar() else {
                    continue;
                };
                let Some(actual) = columns.iter().find(|c| c.name == f.name) else {
                    continue;
                };
                let expected = catalog.types.get(scalar);
                if expected.is_none_or(|t| t.oid != actual.oid) {
                    return Err(Failure::Validation(Value::String(format!(
                        "Return column '{}' has a type mismatch. The expected type is '{}', but the actual type is '{}'.",
                        f.name,
                        expected.map_or(scalar, |t| t.display.as_str()),
                        actual.display
                    ))));
                }
            }
        }
        None => {}
    }
    let arguments = nq
        .arguments
        .iter()
        .map(|(name, a)| {
            let ty = catalog
                .types
                .get(&a.ty)
                .map(|t| t.ty.clone())
                .unwrap_or_else(|| ColumnType::builtin(&a.ty));
            (
                name.clone(),
                NativeQueryArg {
                    ty,
                    nullable: a.nullable,
                    description: a.description.clone(),
                },
            )
        })
        .collect();
    Ok(NativeQueryInfo {
        source: source.clone(),
        root_field: nq.root_field_name.clone(),
        description: nq.description.clone(),
        arguments,
        code,
        returns: logical_model_name(&model.name),
        definition: native_query_definition(nq, catalog),
    })
}

/// V2's `PREPARE _logimo_vali_(<types>) AS …` text.
fn prepare_statement(check: &NativeQueryCheck, catalog: &SourceCatalog) -> String {
    let types: Vec<&str> = check
        .param_types
        .iter()
        .map(|t| {
            catalog
                .types
                .get(t)
                .map_or(t.as_str(), |n| n.display.as_str())
        })
        .collect();
    let params = if types.is_empty() {
        String::new()
    } else {
        format!("({})", types.join(", "))
    };
    format!("PREPARE _logimo_vali_{params} AS {}", check.sql)
}

fn native_query_issue(
    source: &str,
    nq: &NativeQueryMeta,
    catalog: &SourceCatalog,
    failure: Failure,
) -> InconsistentObject {
    let (reason, message) = match failure {
        Failure::Reason(r) => (r, None),
        Failure::Validation(m) => ("Failed to validate query".to_owned(), Some(m)),
    };
    let issue = InconsistentObject::new(
        "native_query",
        format!("native_query {} in source {source}", nq.root_field_name),
        reason,
        native_query_definition(nq, catalog),
    );
    match message {
        Some(m) => issue.with_message(m),
        None => issue,
    }
}

/// The definition V2 reports: argument types canonicalised and `nullable` filled in.
fn native_query_definition(nq: &NativeQueryMeta, catalog: &SourceCatalog) -> Value {
    let mut arguments = Map::new();
    for (name, a) in &nq.arguments {
        let mut arg = Map::new();
        if let Some(d) = &a.description {
            arg.insert("description".into(), json!(d));
        }
        arg.insert("nullable".into(), json!(a.nullable));
        let ty = catalog.types.get(&a.ty).map_or(&a.ty, |t| &t.display);
        arg.insert("type".into(), json!(ty));
        arguments.insert(name.clone(), Value::Object(arg));
    }
    let mut def = Map::new();
    def.insert("arguments".into(), Value::Object(arguments));
    def.insert("code".into(), json!(nq.code));
    if let Some(d) = &nq.description {
        def.insert("description".into(), json!(d));
    }
    def.insert("returns".into(), json!(nq.returns));
    def.insert("root_field_name".into(), json!(nq.root_field_name));
    Value::Object(def)
}

#[cfg(test)]
mod tests;
