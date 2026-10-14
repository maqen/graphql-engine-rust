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

use crate::tables::{ColumnInfo, RelKind, RelationshipInfo, RootFieldNames, TableInfo, Tables};

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
    returns: ReturnsMeta,
    /// `returns` as written, for the definition V2 reports.
    #[serde(skip)]
    returns_json: Value,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    array_relationships: Vec<RelationshipMeta>,
    #[serde(default)]
    object_relationships: Vec<RelationshipMeta>,
}

/// What a native query returns: a logical model by name, or one defined inline (whose
/// types are named after the native query).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ReturnsMeta {
    Name(String),
    Inline(InlineModelMeta),
}

#[derive(Debug, Clone, Deserialize)]
struct InlineModelMeta {
    fields: Vec<LogicalModelField>,
    #[serde(default)]
    select_permissions: Vec<Permission<SelectPermission>>,
}

/// A relationship of a native query, onto a table or another native query.
#[derive(Debug, Clone, Deserialize)]
struct RelationshipMeta {
    name: String,
    using: RelationshipUsing,
}

#[derive(Debug, Clone, Deserialize)]
struct RelationshipUsing {
    column_mapping: IndexMap<String, String>,
    #[serde(default)]
    insertion_order: Option<String>,
    #[serde(default)]
    remote_table: Option<QualifiedTable>,
    #[serde(default)]
    remote_native_query: Option<String>,
}

impl NativeQueryMeta {
    /// The logical model the native query returns: a named one, or its inline one.
    fn model(&self, models: &[LogicalModelMeta]) -> Option<LogicalModelMeta> {
        match &self.returns {
            ReturnsMeta::Name(name) => models.iter().find(|m| &m.name == name).cloned(),
            ReturnsMeta::Inline(inline) => Some(LogicalModelMeta {
                name: self.root_field_name.clone(),
                description: self.description.clone(),
                fields: inline.fields.clone(),
                select_permissions: inline.select_permissions.clone(),
            }),
        }
    }

    /// The key of its logical model in [`Tables`].
    fn model_key(&self) -> QualifiedTable {
        match &self.returns {
            ReturnsMeta::Name(name) => logical_model_name(name),
            ReturnsMeta::Inline(_) => inline_model_name(&self.root_field_name),
        }
    }

    fn relationships(&self) -> impl Iterator<Item = (RelKind, &RelationshipMeta)> {
        let arrays = self.array_relationships.iter().map(|r| (RelKind::Array, r));
        let objects = self
            .object_relationships
            .iter()
            .map(|r| (RelKind::Object, r));
        arrays.chain(objects)
    }
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
    /// The logical model's key in [`Tables`] (see [`logical_model_name`]).
    pub returns: QualifiedTable,
    pub object_relationships: Vec<Arc<RelationshipInfo>>,
    pub array_relationships: Vec<Arc<RelationshipInfo>>,
    /// The metadata as V2 reports it in inconsistencies.
    pub definition: Value,
}

impl NativeQueryInfo {
    pub fn has_relationships(&self) -> bool {
        !self.object_relationships.is_empty() || !self.array_relationships.is_empty()
    }

    /// The GraphQL type of its rows: its logical model's, or with relationships (which
    /// belong to the native query, not the model) a type named after the native query.
    pub fn object_type_name<'a>(&'a self, model: &'a TableInfo) -> &'a str {
        if self.has_relationships() {
            &self.root_field
        } else {
            model.type_name()
        }
    }

    /// The input type of the root field's `args` argument.
    pub fn arguments_type_name(&self) -> String {
        arguments_type_name(&self.root_field)
    }
}

/// The input type of the `args` argument of a field selecting from a native query (its root
/// field, or a relationship onto it).
pub fn arguments_type_name(field: &str) -> String {
    format!("{field}_arguments")
}

/// V2's description of the `args` argument and of its type (sic, no space).
pub fn arguments_description(field: &str) -> String {
    format!("{field}Native Query Arguments")
}

/// The name logical models are registered under in [`Tables`]: an empty schema, which no
/// database table has.
pub fn logical_model_name(name: &str) -> QualifiedTable {
    QualifiedTable::new(String::new(), name.to_owned())
}

/// The key of a native query's inline logical model: a schema no database object or named
/// logical model has (Postgres names cannot contain NUL).
fn inline_model_name(root_field: &str) -> QualifiedTable {
    QualifiedTable::new("\0native_query".to_owned(), root_field.to_owned())
}

fn logical_models(source: &Source) -> Vec<LogicalModelMeta> {
    parse_list(source, "logical_models")
}

fn native_queries(source: &Source) -> Vec<NativeQueryMeta> {
    source
        .extra
        .get("native_queries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let mut nq = NativeQueryMeta::deserialize(v).ok()?;
            nq.returns_json = v.get("returns").cloned().unwrap_or(Value::Null);
            Some(nq)
        })
        .collect()
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

/// Roles named by logical model select permissions, inline ones included (they exist like
/// table permission roles).
pub fn permission_roles(source: &Source) -> impl Iterator<Item = &str> {
    let list = |key: &str| {
        source
            .extra
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let named = list("logical_models");
    let inline = list("native_queries").filter_map(|q| q.get("returns"));
    named
        .chain(inline)
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
        if nq.has_relationships() {
            new_types.push(nq.root_field.clone());
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
    let model = nq.model(models)?;
    let code = parse_code(&nq.code);
    let used = used_arguments(&code);
    let param_types = used
        .iter()
        .map(|a| nq.arguments.get(*a).map(|arg| arg.ty.clone()))
        .collect::<Option<Vec<_>>>()?;
    Some(NativeQueryCheck {
        sql: validation_sql(&code, &model),
        param_types,
    })
}

/// What the backend must resolve for a source: the scalar type names of logical model fields
/// and native query arguments, and the native queries to validate.
pub fn requests(source: &Source) -> (Vec<String>, Vec<NativeQueryCheck>) {
    let models = logical_models(source);
    let queries = native_queries(source);
    let mut types = BTreeSet::new();
    let inline = queries.iter().filter_map(|q| match &q.returns {
        ReturnsMeta::Inline(m) => Some(&m.fields),
        ReturnsMeta::Name(_) => None,
    });
    for fields in models.iter().map(|m| &m.fields).chain(inline) {
        for f in fields {
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
/// `tables.native_queries`) of a source; invalid native queries are reported and skipped, as
/// are relationships onto them (see [`check_dependencies`]).
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
    let register = |tables: &mut Tables, key: QualifiedTable, m: &LogicalModelMeta| {
        let info = Arc::new(logical_model_table(&source_name, key, m, &scalar_type));
        tables
            .by_name
            .insert((source_name.clone(), info.name.clone()), info.clone());
        tables.logical_models.push(info);
    };
    for m in &models {
        register(tables, logical_model_name(&m.name), m);
    }

    let mut resolved = vec![];
    for nq in native_queries(source) {
        match resolve_native_query(&source_name, &nq, &models, catalog) {
            Ok(info) => resolved.push((nq, info)),
            Err(message) => issues.push(native_query_issue(&source.name, &nq, catalog, message)),
        }
    }
    for (nq, _) in &resolved {
        if let (ReturnsMeta::Inline(_), Some(model)) = (&nq.returns, nq.model(&models)) {
            register(tables, nq.model_key(), &model);
        }
    }

    let dropped = check_dependencies(source, catalog, &models, &resolved, tables, issues);
    for (nq, info) in resolved {
        if !dropped.contains(&nq.root_field_name) {
            tables.native_queries.push(Arc::new(info));
        }
    }
}

/// What a relationship's column refers to.
enum Referent {
    /// A field of a native query's logical model.
    NativeQuery { root_field: String, field: String },
    TableColumn {
        table: QualifiedTable,
        column: String,
    },
}

/// Whose relationship refers to it.
#[derive(Clone)]
enum Owner {
    NativeQuery(String),
    TableRelationship {
        table: QualifiedTable,
        name: String,
        kind: RelKind,
    },
}

/// Check what relationships of and onto native queries refer to, as V2 checks its schema
/// dependencies: each column of a relationship's mapping depends on its native query's
/// field or its table's column. A dependency on a missing (or dropped) native query, table
/// or column makes its owner inconsistent: a native query is dropped, which fails the
/// dependencies on it in the next round; a table's relationship is removed from the table.
/// Each failed dependency is reported once, with V2's reason. Returns the dropped native
/// queries.
fn check_dependencies(
    source: &Source,
    catalog: &SourceCatalog,
    models: &[LogicalModelMeta],
    resolved: &[(NativeQueryMeta, NativeQueryInfo)],
    tables: &mut Tables,
    issues: &mut Vec<InconsistentObject>,
) -> HashSet<String> {
    let nq_ref = |root_field: &str, field: &str| Referent::NativeQuery {
        root_field: root_field.to_owned(),
        field: field.to_owned(),
    };
    let mut deps: Vec<(Owner, Referent)> = vec![];
    for (nq, _) in resolved {
        let owner = Owner::NativeQuery(nq.root_field_name.clone());
        for (_, rel) in nq.relationships() {
            for (left, right) in &rel.using.column_mapping {
                deps.push((owner.clone(), nq_ref(&nq.root_field_name, left)));
                let right = match (&rel.using.remote_table, &rel.using.remote_native_query) {
                    (Some(table), _) => Referent::TableColumn {
                        table: table.clone(),
                        column: right.clone(),
                    },
                    (None, Some(target)) => nq_ref(target, right),
                    (None, None) => continue,
                };
                deps.push((owner.clone(), right));
            }
        }
    }
    let source_tables: Vec<Arc<TableInfo>> = tables
        .ordered
        .iter()
        .filter(|t| *t.source == *source.name)
        .cloned()
        .collect();
    for table in &source_tables {
        let rels = table.object_relationships.iter();
        for rel in rels.chain(table.array_relationships.iter()) {
            let Some(target) = &rel.native_query else {
                continue;
            };
            let owner = Owner::TableRelationship {
                table: table.name.clone(),
                name: rel.name.clone(),
                kind: rel.kind,
            };
            for (left, right) in &rel.mapping {
                deps.push((
                    owner.clone(),
                    Referent::TableColumn {
                        table: table.name.clone(),
                        column: left.clone(),
                    },
                ));
                deps.push((owner.clone(), nq_ref(target, right)));
            }
        }
    }

    let fields: std::collections::HashMap<&str, Vec<String>> = resolved
        .iter()
        .map(|(nq, _)| {
            let fields = nq
                .model(models)
                .map(|m| m.fields.into_iter().map(|f| f.name).collect())
                .unwrap_or_default();
            (nq.root_field_name.as_str(), fields)
        })
        .collect();
    let mut dropped: HashSet<String> = HashSet::new();
    let mut removed: Vec<(QualifiedTable, String)> = vec![];
    let mut failed = vec![false; deps.len()];
    loop {
        // Like V2, check every dependency against what was dropped in earlier rounds only.
        let dropped_before = dropped.clone();
        let mut changed = false;
        for (i, (owner, referent)) in deps.iter().enumerate() {
            if failed[i] {
                continue;
            }
            let reason = match referent {
                Referent::NativeQuery { root_field, field } => {
                    match fields.get(root_field.as_str()) {
                        Some(_) if dropped_before.contains(root_field) => {
                            Some(format!("native query \"{root_field}\" is not tracked"))
                        }
                        Some(fs) if !fs.contains(field) => Some(format!(
                            "native query \"{root_field}\" has no field named \"{field}\""
                        )),
                        Some(_) => None,
                        None => Some(format!("native query \"{root_field}\" is not tracked")),
                    }
                }
                Referent::TableColumn { table, column } => match tables.get(&source.name, table) {
                    None => Some(format!("table \"{}\" is not tracked", table.v2_text())),
                    Some(t) if !t.columns.contains_key(column) => Some(format!(
                        "table \"{}\" has no field named \"{column}\"",
                        table.v2_text()
                    )),
                    Some(_) => None,
                },
            };
            let Some(reason) = reason else {
                continue;
            };
            failed[i] = true;
            changed = true;
            match owner {
                Owner::NativeQuery(root_field) => {
                    dropped.insert(root_field.clone());
                    let (nq, _) = resolved
                        .iter()
                        .find(|(nq, _)| nq.root_field_name == *root_field)
                        .expect("owners are resolved native queries");
                    issues.push(native_query_issue(
                        &source.name,
                        nq,
                        catalog,
                        Failure::Reason(reason),
                    ));
                }
                Owner::TableRelationship { table, name, kind } => {
                    let entry = &tables.get(&source.name, table).expect("owner table").entry;
                    let (kind_name, definition) = match kind {
                        RelKind::Object => (
                            "object_relation",
                            entry
                                .object_relationships
                                .iter()
                                .find(|r| r.name == *name)
                                .map(serde_json::to_value),
                        ),
                        RelKind::Array => (
                            "array_relation",
                            entry
                                .array_relationships
                                .iter()
                                .find(|r| r.name == *name)
                                .map(serde_json::to_value),
                        ),
                    };
                    let definition = definition.and_then(Result::ok).unwrap_or_default();
                    issues.push(crate::tables::relationship_issue(
                        kind_name,
                        &definition,
                        name,
                        &source.name,
                        table,
                        reason,
                    ));
                    removed.push((table.clone(), name.clone()));
                }
            }
        }
        if !changed {
            break;
        }
    }

    for (table, name) in removed {
        let key = (Arc::from(source.name.as_str()), table);
        let Some(info) = tables.by_name.get(&key) else {
            continue;
        };
        let mut info = (**info).clone();
        info.object_relationships.retain(|r| r.name != name);
        info.array_relationships.retain(|r| r.name != name);
        let info = Arc::new(info);
        for t in tables.ordered.iter_mut() {
            if t.source == info.source && t.name == info.name {
                *t = info.clone();
            }
        }
        tables.by_name.insert(key, info);
    }
    dropped
}

fn logical_model_table(
    source: &Arc<str>,
    name: QualifiedTable,
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
    let Some(model) = nq.model(models) else {
        let name = match &nq.returns {
            ReturnsMeta::Name(name) => name.as_str(),
            ReturnsMeta::Inline(_) => "",
        };
        return Err(Failure::Reason(format!(
            "The logical model {name} could not be found"
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
    let mut duplicates: Vec<&str> = nq
        .array_relationships
        .iter()
        .filter(|a| nq.object_relationships.iter().any(|o| o.name == a.name))
        .map(|a| a.name.as_str())
        .collect();
    if !duplicates.is_empty() {
        duplicates.sort_unstable();
        duplicates.dedup();
        return Err(Failure::Reason(format!(
            "The native query '{}' has duplicate relationships: {}",
            nq.root_field_name,
            duplicates.join(",")
        )));
    }
    let mut object_relationships = vec![];
    let mut array_relationships = vec![];
    for (kind, rel) in nq.relationships() {
        let (target, native_query) = match (&rel.using.remote_table, &rel.using.remote_native_query)
        {
            (Some(table), _) => (table.clone(), None),
            (None, Some(target)) => (
                QualifiedTable::new(String::new(), String::new()),
                Some(target.clone()),
            ),
            (None, None) => {
                return Err(Failure::Reason("key \"remote_table\" not found".to_owned()));
            }
        };
        let info = Arc::new(RelationshipInfo {
            name: rel.name.clone(),
            kind,
            source: source.clone(),
            target,
            native_query,
            mapping: rel
                .using
                .column_mapping
                .iter()
                .map(|(l, r)| (l.clone(), r.clone()))
                .collect(),
            nullable: true,
            description: None,
        });
        match kind {
            RelKind::Object => object_relationships.push(info),
            RelKind::Array => array_relationships.push(info),
        }
    }
    Ok(NativeQueryInfo {
        source: source.clone(),
        root_field: nq.root_field_name.clone(),
        description: nq.description.clone(),
        arguments,
        code,
        returns: nq.model_key(),
        object_relationships,
        array_relationships,
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
    // V2 lists relationships by name.
    let relationships = |rels: &[RelationshipMeta]| {
        let mut rels: Vec<&RelationshipMeta> = rels.iter().collect();
        rels.sort_by(|a, b| a.name.cmp(&b.name));
        let rels = rels.into_iter().map(|r| {
            let mut using = Map::new();
            using.insert("column_mapping".into(), json!(r.using.column_mapping));
            using.insert("insertion_order".into(), json!(r.using.insertion_order));
            match (&r.using.remote_table, &r.using.remote_native_query) {
                (Some(table), _) => using.insert("remote_table".into(), json!(table)),
                (None, nq) => using.insert("remote_native_query".into(), json!(nq)),
            };
            json!({"name": r.name, "using": using})
        });
        Value::Array(rels.collect())
    };
    let mut def = Map::new();
    def.insert("arguments".into(), Value::Object(arguments));
    if !nq.array_relationships.is_empty() {
        def.insert(
            "array_relationships".into(),
            relationships(&nq.array_relationships),
        );
    }
    def.insert("code".into(), json!(nq.code));
    if let Some(d) = &nq.description {
        def.insert("description".into(), json!(d));
    }
    if !nq.object_relationships.is_empty() {
        def.insert(
            "object_relationships".into(),
            relationships(&nq.object_relationships),
        );
    }
    def.insert("returns".into(), nq.returns_json.clone());
    def.insert("root_field_name".into(), json!(nq.root_field_name));
    Value::Object(def)
}

#[cfg(test)]
mod tests;
