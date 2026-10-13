//! Tracked functions: SQL functions returning rows of a tracked table, exposed as root fields
//! (queries, or mutations for `VOLATILE` functions).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hge_core::{
    ColumnType, FunctionArg, InconsistentObject, QualifiedFunction, QualifiedTable, RoleName,
    SourceCatalog, TypeKind, Volatility,
};
use hge_ir::{FunctionArgValue, FunctionCall, Value};
use hge_metadata::{InheritedRole, Source};
use serde::Deserialize;

use super::{build_call, input_args, is_json, lookup};
use crate::tables::{TableInfo, Tables};

/// `functions[]` of a source.
#[derive(Debug, Clone, Deserialize)]
struct TrackedFunctionMeta {
    function: QualifiedFunction,
    #[serde(default)]
    configuration: FunctionConfiguration,
    #[serde(default)]
    permissions: Vec<FunctionPermission>,
    #[serde(default)]
    comment: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FunctionConfiguration {
    #[serde(default)]
    custom_name: Option<String>,
    #[serde(default)]
    custom_root_fields: Option<FunctionRootFields>,
    #[serde(default)]
    session_argument: Option<String>,
    #[serde(default)]
    exposed_as: Option<ExposedAs>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FunctionRootFields {
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    function_aggregate: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FunctionPermission {
    role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposedAs {
    Query,
    Mutation,
}

/// A resolved tracked function.
#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub source: Arc<str>,
    pub function: QualifiedFunction,
    /// `custom_name`, else the function name (`schema_name` outside `public`); type names
    /// (`<name>_args`) derive from it.
    pub gql_name: String,
    /// The root field (`custom_root_fields.function`, else `gql_name`).
    pub root_field: String,
    /// `<gql_name>_aggregate` (or `custom_root_fields.function_aggregate`), for query functions.
    pub aggregate_root_field: String,
    pub description: String,
    pub args: Vec<FunctionArg>,
    pub session_arg: Option<usize>,
    /// The tracked table the function returns rows of.
    pub returns: QualifiedTable,
    /// `SETOF table` (a list) rather than a single row (nullable object).
    pub returns_set: bool,
    pub exposed_as: ExposedAs,
    /// Roles with an explicit function permission (inherited roles included).
    pub permitted: HashSet<RoleName>,
}

impl FunctionInfo {
    pub fn input_args(&self) -> impl Iterator<Item = (usize, String, &FunctionArg)> + '_ {
        input_args(&self.args, move |i| Some(i) == self.session_arg)
    }

    pub fn has_input_args(&self) -> bool {
        self.input_args().next().is_some()
    }

    /// `args` is required when some input argument has no default.
    pub fn args_required(&self) -> bool {
        self.input_args().any(|(_, _, a)| !a.has_default)
    }

    pub fn args_type_name(&self) -> String {
        format!("{}_args", self.gql_name)
    }

    pub fn call(&self, inputs: Vec<(usize, Value)>) -> Result<FunctionCall, &'static str> {
        let implicit = self
            .session_arg
            .map(|i| (i, FunctionArgValue::Session))
            .into_iter()
            .collect();
        build_call(&self.function, &self.args, implicit, inputs)
    }

    /// Whether `role` may call the function, given it can select from the returned table.
    /// Query functions are permitted to every such role unless `infer` is off
    /// (`HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS=false`); mutations always need a
    /// function permission.
    pub fn permits(&self, role: &RoleName, infer: bool) -> bool {
        role.is_admin()
            || (infer && self.exposed_as == ExposedAs::Query)
            || self.permitted.contains(role)
    }
}

/// Resolve the tracked functions of one source (after its tables). Unresolvable functions are
/// reported and skipped.
pub fn resolve(
    source: &Source,
    catalog: &SourceCatalog,
    tables: &Tables,
    inherited_roles: &[InheritedRole],
    issues: &mut Vec<InconsistentObject>,
) -> Vec<Arc<FunctionInfo>> {
    let mut out = vec![];
    for raw in &source.functions {
        let meta = match TrackedFunctionMeta::deserialize(raw) {
            Ok(m) => m,
            Err(e) => {
                let name = raw.get("function").cloned().unwrap_or_default();
                issues.push(InconsistentObject::new(
                    "function",
                    format!("function {name} in source {}", source.name),
                    e.to_string(),
                    name,
                ));
                continue;
            }
        };
        match resolve_one(source, &meta, catalog, tables, inherited_roles) {
            Ok(f) => out.push(Arc::new(f)),
            Err(reason) => issues.push(InconsistentObject::new(
                "function",
                format!(
                    "function {} in source {}",
                    meta.function.v2_text(),
                    source.name
                ),
                reason,
                serde_json::to_value(&meta.function).unwrap_or_default(),
            )),
        }
    }
    out
}

/// Drop functions whose root fields are already defined by a table or an earlier function on
/// the same root, or whose `<name>_args` type clashes with a different definition. V2 rejects
/// the whole schema in that case ("Encountered conflicting definitions …"); here only the
/// function is left out, with V2's message as the reason. `tables` must not hold functions yet.
pub(crate) fn drop_conflicting(
    functions: Vec<Arc<FunctionInfo>>,
    tables: &Tables,
    issues: &mut Vec<InconsistentObject>,
) -> Vec<Arc<FunctionInfo>> {
    let names = crate::build::table_schema_names(tables);
    let table_owner = |field: &str| {
        tables.ordered.iter().find_map(|t| {
            let rf = &t.root_fields;
            let stream = crate::subscription::stream_field_name(t);
            [&rf.select, &rf.select_by_pk, &rf.select_aggregate, &stream]
                .contains(&&field.to_owned())
                .then(|| format!("table {} in source {}", t.name.v2_text(), t.source))
        })
    };
    // Query functions live on the query and subscription roots; V2 reports the latter.
    let mut query_fields: HashMap<String, String> = HashMap::new();
    let mut mutation_fields: HashMap<String, String> = HashMap::new();
    // `<name>_args` types already defined, with their fields and the root field defining them.
    let mut arg_types: HashMap<String, (Vec<(String, ColumnType)>, String)> = HashMap::new();
    let mut kept = Vec::with_capacity(functions.len());
    for f in functions {
        let (root, taken) = match f.exposed_as {
            ExposedAs::Query => ("subscription_root", &mut query_fields),
            ExposedAs::Mutation => ("mutation_root", &mut mutation_fields),
        };
        let table_field = |n: &String| match f.exposed_as {
            ExposedAs::Query => {
                names.query_fields.contains(n) || names.subscription_fields.contains(n)
            }
            ExposedAs::Mutation => names.mutation_fields.contains(n),
        };
        let mut root_fields = vec![f.root_field.clone()];
        if f.exposed_as == ExposedAs::Query {
            root_fields.push(f.aggregate_root_field.clone());
        }
        let parent = match f.exposed_as {
            ExposedAs::Query => "query_root",
            ExposedAs::Mutation => "mutation_root",
        };
        let arg_fields: Vec<(String, ColumnType)> = f
            .input_args()
            .map(|(_, name, a)| (name, a.ty.clone()))
            .collect();
        let args_type = f.has_input_args().then(|| f.args_type_name());
        let reason = if let Some(field) = root_fields
            .iter()
            .find(|n| table_field(n) || taken.contains_key(*n))
        {
            let origin = table_owner(field)
                .filter(|_| table_field(field))
                .map(|o| format!(" defined in [{o}] and of unknown origin"))
                .unwrap_or_default();
            Some(format!(
                "Encountered conflicting definitions in the selection set for '{root}' for \
                 field '{field}'{origin}. Fields must not be defined more than once across \
                 all sources."
            ))
        } else if let Some(t) = &args_type {
            let here = format!("{parent}.{}.args", f.root_field);
            match arg_types.get(t) {
                // The same definition is shared, as in V2.
                Some((fields, _)) if *fields == arg_fields => None,
                Some((_, there)) => Some(format!(
                    "Found conflicting definitions for GraphQL type '{t}'.  The definition at \
                     '{here}' differs from the definition at e.g. '{there}'."
                )),
                None if names.types.contains_key(t) => Some(format!(
                    "Found conflicting definitions for GraphQL type '{t}'.  The definition at \
                     '{here}' differs from the definition of a table type."
                )),
                None => None,
            }
        } else {
            None
        };
        match reason {
            Some(reason) => issues.push(InconsistentObject::new(
                "function",
                format!("function {} in source {}", f.function.v2_text(), f.source),
                reason,
                serde_json::to_value(&f.function).unwrap_or_default(),
            )),
            None => {
                for n in root_fields {
                    taken.insert(n, f.function.v2_text());
                }
                if let Some(t) = args_type {
                    let here = format!("{parent}.{}.args", f.root_field);
                    arg_types.entry(t).or_insert((arg_fields, here));
                }
                kept.push(f);
            }
        }
    }
    kept
}

fn resolve_one(
    source: &Source,
    meta: &TrackedFunctionMeta,
    catalog: &SourceCatalog,
    tables: &Tables,
    inherited_roles: &[InheritedRole],
) -> Result<FunctionInfo, String> {
    let fname = meta.function.v2_text();
    let scoped = |m: String| format!("in function \"{fname}\": {m}");
    let cannot = |why: &str| {
        scoped(format!(
            "the function \"{fname}\" cannot be tracked because {why}"
        ))
    };
    let f = lookup(catalog, &meta.function).map_err(scoped)?;
    if f.variadic {
        return Err(cannot(
            "function with \"VARIADIC\" parameters are not supported",
        ));
    }
    let ret = &f.return_type;
    if ret.kind != TypeKind::Composite || ret.dimensions > 0 {
        return Err(scoped(format!(
            "the function \"{fname}\" cannot be tracked for the following reasons:\n  • the function does not return a \"COMPOSITE\" type\n  • the function does not return a table\n"
        )));
    }
    let returns = QualifiedTable::new(ret.schema.clone(), ret.name.clone());
    let table: &Arc<TableInfo> = tables
        .get(&source.name, &returns)
        .ok_or_else(|| format!("table \"{}\" is not tracked", returns.v2_text()))?;

    let config = &meta.configuration;
    let session_arg = match &config.session_argument {
        Some(arg) => {
            let i = f
                .args
                .iter()
                .position(|a| a.name.as_deref() == Some(arg))
                .ok_or_else(|| {
                    cannot(&format!(
                        "given session argument \"{arg}\" not the input argument of the function"
                    ))
                })?;
            if !is_json(&f.args[i]) {
                return Err(cannot(&format!(
                    "given session argument \"{arg}\" is not of type json"
                )));
            }
            Some(i)
        }
        None => None,
    };
    let volatile = f.volatility == Volatility::Volatile;
    let exposed_as = match config.exposed_as {
        Some(ExposedAs::Mutation) if !volatile => {
            return Err(cannot(
                "the function was requested to be exposed as a mutation, but is not marked VOLATILE. Maybe the function was given the wrong volatility when it was defined?",
            ));
        }
        Some(e) => e,
        None if volatile => ExposedAs::Mutation,
        None => ExposedAs::Query,
    };

    let gql_name = config.custom_name.clone().unwrap_or_else(|| {
        if meta.function.schema == "public" {
            meta.function.name.clone()
        } else {
            format!("{}_{}", meta.function.schema, meta.function.name)
        }
    });
    let root_field = config
        .custom_root_fields
        .as_ref()
        .and_then(|r| r.function.clone())
        .unwrap_or_else(|| gql_name.clone());
    let aggregate_root_field = config
        .custom_root_fields
        .as_ref()
        .and_then(|r| r.function_aggregate.clone())
        .unwrap_or_else(|| format!("{gql_name}_aggregate"));
    let description = meta.comment.clone().unwrap_or_else(|| {
        let kind = if exposed_as == ExposedAs::Mutation {
            "VOLATILE function"
        } else {
            "function"
        };
        format!(
            "execute {kind} \"{fname}\" which returns \"{}\"",
            table.display_name()
        )
    });

    // Inherited roles get a function permission any of their parents has.
    let mut permitted: HashSet<RoleName> = meta
        .permissions
        .iter()
        .map(|p| RoleName::new(&p.role))
        .collect();
    loop {
        let before = permitted.len();
        for r in inherited_roles {
            if r.role_set
                .iter()
                .any(|p| permitted.contains(&RoleName::new(p)))
            {
                permitted.insert(RoleName::new(&r.role_name));
            }
        }
        if permitted.len() == before {
            break;
        }
    }

    Ok(FunctionInfo {
        source: Arc::from(source.name.as_str()),
        function: meta.function.clone(),
        gql_name,
        root_field,
        aggregate_root_field,
        description,
        args: f.args.clone(),
        session_arg,
        returns,
        returns_set: f.returns_set,
        exposed_as,
        permitted,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hge_core::{
        ColumnInfo, ColumnType, FunctionCatalog, TableCatalog, TableKind, TableMutability,
    };
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

    fn function(name: &str, args: Vec<(&str, ColumnType)>, ret: ColumnType) -> FunctionCatalog {
        FunctionCatalog {
            name: QualifiedFunction::public(name),
            description: None,
            args: args
                .into_iter()
                .map(|(n, ty)| FunctionArg {
                    name: Some(n.into()),
                    ty,
                    has_default: false,
                })
                .collect(),
            return_type: ret,
            returns_set: true,
            volatility: Volatility::Stable,
            variadic: false,
        }
    }

    /// Resolve `functions` (metadata) against a catalog with a tracked `author` table.
    fn resolve_functions(functions: serde_json::Value) -> (Tables, Vec<String>) {
        let author = QualifiedTable::public("author");
        let row = ty("public", "author", TypeKind::Composite);
        let int = ty("pg_catalog", "int4", TypeKind::Base);
        let json_ty = ty("pg_catalog", "json", TypeKind::Base);
        let mut variadic = function("var", vec![("xs", int.clone())], row.clone());
        variadic.variadic = true;
        let mut volatile = function("vol", vec![], row.clone());
        volatile.volatility = Volatility::Volatile;
        let fns = vec![
            function("ok", vec![("s", json_ty), ("x", int.clone())], row.clone()),
            function(
                "ok2",
                vec![("x", ty("pg_catalog", "text", TypeKind::Base))],
                row.clone(),
            ),
            function("scalar", vec![], int.clone()),
            function("other", vec![], ty("public", "other", TypeKind::Composite)),
            variadic,
            volatile,
        ];
        let table = TableCatalog {
            name: author.clone(),
            kind: TableKind::Table,
            description: None,
            columns: [(
                "id".to_owned(),
                ColumnInfo {
                    name: "id".into(),
                    ty: int,
                    nullable: false,
                    has_default: false,
                    is_identity: false,
                    is_identity_always: false,
                    is_generated: false,
                    description: None,
                },
            )]
            .into_iter()
            .collect(),
            primary_key: None,
            unique_constraints: vec![],
            foreign_keys: vec![],
            constraints: vec![],
            mutability: TableMutability::ALL,
        };
        let mut functions_cat: HashMap<QualifiedFunction, Vec<FunctionCatalog>> = HashMap::new();
        for f in fns {
            functions_cat.entry(f.name.clone()).or_default().push(f);
        }
        let catalog = SourceCatalog {
            tables: HashMap::from([(author, table)]),
            enums: HashMap::new(),
            functions: functions_cat,
        };
        let metadata: hge_metadata::Metadata = serde_json::from_value(json!({
            "version": 3,
            "sources": [{
                "name": "default", "kind": "postgres",
                "configuration": {"connection_info": {"database_url": "postgres://x"}},
                "tables": [{"table": "author", "select_permissions": [
                    {"role": "a", "permission": {"columns": ["id"], "filter": {}}},
                    {"role": "b", "permission": {"columns": ["id"], "filter": {}}},
                ]}],
                "functions": functions,
            }],
            "inherited_roles": [{"role_name": "ab", "role_set": ["a", "b"]}],
        }))
        .unwrap();
        let (tables, issues) = crate::resolve_tables(
            &metadata,
            &[("default".to_owned(), catalog)].into_iter().collect(),
        );
        (tables, issues.into_iter().map(|i| i.reason).collect())
    }

    #[test]
    fn resolves_names_session_argument_and_permissions() {
        let (tables, issues) = resolve_functions(json!([
            {"function": "ok", "configuration": {"session_argument": "s", "custom_name": "okay",
                "custom_root_fields": {"function": "run_ok"}},
             "permissions": [{"role": "a"}]},
            {"function": "vol"},
        ]));
        assert!(issues.is_empty(), "{issues:?}");
        let ok = &tables.functions[0];
        assert_eq!(
            (ok.gql_name.as_str(), ok.root_field.as_str()),
            ("okay", "run_ok")
        );
        assert_eq!(ok.args_type_name(), "okay_args");
        assert_eq!(ok.session_arg, Some(0));
        assert!(ok.args_required());
        assert!(ok.permitted.contains(&RoleName::new("ab")));
        assert!(ok.permits(&RoleName::new("c"), true));
        assert!(!ok.permits(&RoleName::new("c"), false));
        let vol = &tables.functions[1];
        assert_eq!(vol.exposed_as, ExposedAs::Mutation);
        assert_eq!(
            vol.description,
            "execute VOLATILE function \"vol\" which returns \"author\""
        );
        assert!(!vol.permits(&RoleName::new("c"), true));
    }

    #[test]
    fn reports_v2_errors() {
        let (_, issues) = resolve_functions(json!([
            {"function": "nope"},
            {"function": "scalar"},
            {"function": "other"},
            {"function": "var"},
            {"function": "ok", "configuration": {"session_argument": "x"}},
            {"function": "ok", "configuration": {"session_argument": "zz"}},
            {"function": "ok", "configuration": {"exposed_as": "mutation"}},
        ]));
        assert_eq!(
            issues,
            [
                "in function \"nope\": no such function exists: \"nope\"",
                "in function \"scalar\": the function \"scalar\" cannot be tracked for the following reasons:\n  • the function does not return a \"COMPOSITE\" type\n  • the function does not return a table\n",
                "table \"other\" is not tracked",
                "in function \"var\": the function \"var\" cannot be tracked because function with \"VARIADIC\" parameters are not supported",
                "in function \"ok\": the function \"ok\" cannot be tracked because given session argument \"x\" is not of type json",
                "in function \"ok\": the function \"ok\" cannot be tracked because given session argument \"zz\" not the input argument of the function",
                "in function \"ok\": the function \"ok\" cannot be tracked because the function was requested to be exposed as a mutation, but is not marked VOLATILE. Maybe the function was given the wrong volatility when it was defined?",
            ]
        );
    }

    #[test]
    fn drops_functions_whose_names_conflict() {
        let (tables, issues) = resolve_functions(json!([
            {"function": "ok", "configuration": {"custom_root_fields": {"function": "author_aggregate"}}},
            {"function": "ok", "configuration": {"custom_root_fields": {"function": "author_stream"}}},
            {"function": "ok", "configuration": {"custom_name": "author"}},
            {"function": "ok", "configuration": {"custom_name": "first"}},
            {"function": "ok", "configuration": {"custom_name": "first"}},
            // A mutation may share a name with a query function.
            {"function": "vol", "configuration": {"custom_root_fields": {"function": "first"}}},
            // Identical `<name>_args` definitions are shared; different ones conflict.
            {"function": "ok", "configuration": {"custom_name": "first",
                "custom_root_fields": {"function": "again", "function_aggregate": "again_agg"}}},
            {"function": "ok2", "configuration": {"custom_name": "first",
                "custom_root_fields": {"function": "other", "function_aggregate": "other_agg"}}},
        ]));
        let kept: Vec<&str> = tables
            .functions
            .iter()
            .map(|f| f.root_field.as_str())
            .collect();
        assert_eq!(kept, ["first", "first", "again"]);
        assert_eq!(
            issues,
            [
                "Encountered conflicting definitions in the selection set for 'subscription_root' for field 'author_aggregate' defined in [table author in source default] and of unknown origin. Fields must not be defined more than once across all sources.",
                "Encountered conflicting definitions in the selection set for 'subscription_root' for field 'author_stream' defined in [table author in source default] and of unknown origin. Fields must not be defined more than once across all sources.",
                "Encountered conflicting definitions in the selection set for 'subscription_root' for field 'author' defined in [table author in source default] and of unknown origin. Fields must not be defined more than once across all sources.",
                "Encountered conflicting definitions in the selection set for 'subscription_root' for field 'first'. Fields must not be defined more than once across all sources.",
                "Found conflicting definitions for GraphQL type 'first_args'.  The definition at 'query_root.other.args' differs from the definition at e.g. 'query_root.first.args'.",
            ]
        );
    }
}
