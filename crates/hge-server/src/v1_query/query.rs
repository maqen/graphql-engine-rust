//! The `/v1/query` envelope: `{"type": "...", "args": ..., "version": 1 | 2}`.

use serde_json::Value as Json;

use hge_metadata_api::api::Command;
use hge_metadata_api::run_sql::RunSql;

use super::count::CountQuery;
use super::delete::DeleteQuery;
use super::insert::InsertQuery;
use super::json::{Obj, Path, parse_failed};
use super::select::SelectQuery;
use super::update::UpdateQuery;
use hge_metadata_api::ApiError;

/// `type`s of version 1 queries, in V2's order (its parse error lists them).
const V1_TYPES: &[&str] = &[
    "add_existing_table_or_view",
    "track_table",
    "untrack_table",
    "set_table_is_enum",
    "set_table_customization",
    "track_function",
    "untrack_function",
    "create_object_relationship",
    "create_array_relationship",
    "drop_relationship",
    "set_relationship_comment",
    "rename_relationship",
    "add_computed_field",
    "drop_computed_field",
    "create_remote_relationship",
    "update_remote_relationship",
    "delete_remote_relationship",
    "create_insert_permission",
    "create_select_permission",
    "create_update_permission",
    "create_delete_permission",
    "drop_insert_permission",
    "drop_select_permission",
    "drop_update_permission",
    "drop_delete_permission",
    "set_permission_comment",
    "get_inconsistent_metadata",
    "drop_inconsistent_metadata",
    "insert",
    "select",
    "update",
    "delete",
    "count",
    "bulk",
    "add_remote_schema",
    "update_remote_schema",
    "remove_remote_schema",
    "reload_remote_schema",
    "introspect_remote_schema",
    "create_event_trigger",
    "delete_event_trigger",
    "redeliver_event",
    "invoke_event_trigger",
    "create_cron_trigger",
    "delete_cron_trigger",
    "create_scheduled_event",
    "create_query_collection",
    "rename_query_collection",
    "drop_query_collection",
    "add_query_to_collection",
    "drop_query_from_collection",
    "add_collection_to_allowlist",
    "drop_collection_from_allowlist",
    "run_sql",
    "replace_metadata",
    "export_metadata",
    "clear_metadata",
    "reload_metadata",
    "create_action",
    "drop_action",
    "update_action",
    "create_action_permission",
    "drop_action_permission",
    "create_rest_endpoint",
    "drop_rest_endpoint",
    "dump_internal_state",
    "set_custom_types",
];

/// `type`s of version 2 queries.
const V2_TYPES: &[&str] = &[
    "track_table",
    "set_table_custom_fields",
    "track_function",
    "replace_metadata",
];

/// Types any role may run (with its table permissions); everything else is admin only.
const DML_TYPES: &[&str] = &["insert", "select", "update", "delete", "count"];

/// Legacy types that are Postgres metadata commands (`track_table` → `pg_track_table`);
/// the other metadata types keep their name on `/v1/metadata`.
const PG_TYPES: &[&str] = &[
    "track_table",
    "untrack_table",
    "set_table_is_enum",
    "set_table_customization",
    "set_table_custom_fields",
    "track_function",
    "untrack_function",
    "create_object_relationship",
    "create_array_relationship",
    "drop_relationship",
    "set_relationship_comment",
    "rename_relationship",
    "add_computed_field",
    "drop_computed_field",
    "create_remote_relationship",
    "update_remote_relationship",
    "delete_remote_relationship",
    "create_insert_permission",
    "create_select_permission",
    "create_update_permission",
    "create_delete_permission",
    "drop_insert_permission",
    "drop_select_permission",
    "drop_update_permission",
    "drop_delete_permission",
    "set_permission_comment",
    "create_event_trigger",
    "delete_event_trigger",
    "redeliver_event",
    "invoke_event_trigger",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

/// A parsed `/v1/query` request.
#[derive(Debug, Clone)]
pub enum Query {
    Select(SelectQuery),
    Count(CountQuery),
    Insert(InsertQuery),
    Update(UpdateQuery),
    Delete(DeleteQuery),
    /// Sub-queries run in order; the first error aborts the request.
    Bulk(Vec<Query>),
    RunSql {
        args: RunSql,
        /// Where the query's `args` sit (`$.args`, `$.args[1].args`).
        path: Path,
    },
    /// A metadata command, run like on `/v1/metadata`.
    Metadata {
        command: Command,
        /// Where the query's envelope sits (`$`, `$.args[1]`).
        path: Path,
    },
    /// A type this endpoint does not run yet.
    Other {
        ty: String,
        path: Path,
    },
}

impl Query {
    /// Parse a request envelope found at `path` (`$`, or `$.args[i]` inside a bulk).
    pub fn parse(v: &Json, path: &Path) -> Result<Query, ApiError> {
        let o = Obj::new(v, path.clone(), "Object")?;
        let version = match o.opt("version") {
            None => Version::V1,
            Some(Json::Number(n)) if n.as_u64() == Some(1) => Version::V1,
            Some(Json::Number(n)) if n.as_u64() == Some(2) => Version::V2,
            Some(other) => {
                return Err(parse_failed(
                    format!("expected 1 or 2, encountered {other}"),
                    &o.at("version"),
                ));
            }
        };
        let (record, types) = match version {
            Version::V1 => ("Hasura.Server.API.Query.RQLQueryV1", V1_TYPES),
            Version::V2 => ("Hasura.Server.API.Query.RQLQueryV2", V2_TYPES),
        };
        let tag_list = || {
            types
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(",")
        };
        let ty = match o.get("type") {
            Some(Json::String(t)) => t.as_str(),
            Some(other) => {
                return Err(parse_failed(
                    format!(
                        "parsing {record} failed, expected Object with key \"type\" containing one of [{}], parsing Text failed, expected String, but encountered {}",
                        tag_list(),
                        super::json::kind(other)
                    ),
                    &o.at("type"),
                ));
            }
            None => {
                return Err(parse_failed(
                    format!(
                        "parsing {record} failed, expected Object with key \"type\" containing one of [{}], key \"type\" not found",
                        tag_list()
                    ),
                    path,
                ));
            }
        };
        if !types.contains(&ty) {
            return Err(parse_failed(
                format!(
                    "parsing {record} failed, expected tag field to be one of [{}], but found tag \"{ty}\"",
                    tag_list()
                ),
                &o.at("type"),
            ));
        }
        let args_path = o.at("args");
        let args = o.get("args").ok_or_else(|| {
            parse_failed(
                format!(
                    "parsing {record}({}) failed, key \"args\" not found",
                    constructor(version, ty)
                ),
                path,
            )
        })?;
        if version == Version::V1 && ty == "run_sql" {
            return serde_json::from_value(args.clone())
                .map(|args| Query::RunSql {
                    args,
                    path: args_path.clone(),
                })
                .map_err(|e| {
                    parse_failed(hge_metadata_api::response::aeson_message(&e), &args_path)
                });
        }
        if version == Version::V2 || !DML_TYPES.contains(&ty) && ty != "bulk" {
            return Ok(Query::Metadata {
                command: Command {
                    name: metadata_command(ty),
                    args: args.clone(),
                    version: match version {
                        Version::V1 => 1,
                        Version::V2 => 2,
                    },
                },
                path: path.clone(),
            });
        }
        Ok(match (version, ty) {
            (Version::V1, "select") => Query::Select(SelectQuery::parse(args, &args_path)?),
            (Version::V1, "count") => Query::Count(CountQuery::parse(args, &args_path)?),
            (Version::V1, "insert") => Query::Insert(InsertQuery::parse(args, &args_path)?),
            (Version::V1, "update") => Query::Update(UpdateQuery::parse(args, &args_path)?),
            (Version::V1, "delete") => Query::Delete(DeleteQuery::parse(args, &args_path)?),
            (Version::V1, "bulk") => match args {
                Json::Array(items) => Query::Bulk(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, q)| Query::parse(q, &args_path.index(i)))
                        .collect::<Result<_, _>>()?,
                ),
                other => {
                    return Err(super::json::type_mismatch("[]", "Array", other, &args_path));
                }
            },
            _ => Query::Other {
                ty: ty.to_owned(),
                path: args_path,
            },
        })
    }

    /// V2's `requiresAdmin`.
    pub fn requires_admin(&self) -> bool {
        match self {
            Query::Select(_)
            | Query::Count(_)
            | Query::Insert(_)
            | Query::Update(_)
            | Query::Delete(_) => false,
            Query::RunSql { .. } | Query::Metadata { .. } => true,
            Query::Bulk(qs) => qs.iter().any(Query::requires_admin),
            Query::Other { ty, .. } => !DML_TYPES.contains(&ty.as_str()),
        }
    }

    /// Whether the request needs a metadata unit of work (V2's `queryModifiesSchemaCache`),
    /// and whether that one may write (so takes the metadata lock).
    pub fn metadata_access(&self) -> Option<bool> {
        match self {
            Query::RunSql { args, .. } => args.checks_metadata().then_some(true),
            Query::Metadata { command, .. } => {
                Some(hge_metadata_api::api::modifies_metadata(command))
            }
            Query::Bulk(qs) => qs
                .iter()
                .filter_map(Query::metadata_access)
                .reduce(|a, b| a || b),
            Query::Select(_)
            | Query::Count(_)
            | Query::Insert(_)
            | Query::Update(_)
            | Query::Delete(_)
            | Query::Other { .. } => None,
        }
    }

    /// V2's `queryModifiesUserDB` (refused in read-only mode).
    pub fn writes_data(&self) -> bool {
        match self {
            Query::RunSql { args, .. } => !args.read_only,
            Query::Bulk(qs) => qs.iter().any(Query::writes_data),
            Query::Insert(_) | Query::Update(_) | Query::Delete(_) => true,
            Query::Other { ty, .. } => matches!(ty.as_str(), "insert" | "update" | "delete"),
            Query::Select(_) | Query::Count(_) | Query::Metadata { .. } => false,
        }
    }
}

/// The `/v1/metadata` command for a legacy metadata type.
fn metadata_command(ty: &str) -> String {
    match ty {
        "add_existing_table_or_view" => "pg_track_table".to_owned(),
        t if PG_TYPES.contains(&t) => format!("pg_{t}"),
        t => t.to_owned(),
    }
}

/// The Haskell constructor of a query type (`run_sql` → `RQRunSql`), as aeson names it.
fn constructor(version: Version, ty: &str) -> String {
    let mut out = String::from(match version {
        Version::V1 => "RQ",
        Version::V2 => "RQV2",
    });
    for word in ty.split('_') {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_type_lists_tags() {
        let e = Query::parse(&json!({"type": "random", "args": {}}), &Path::root()).unwrap_err();
        assert_eq!(e.error.path, "$.type");
        assert!(e.error.message.starts_with(
            "parsing Hasura.Server.API.Query.RQLQueryV1 failed, expected tag field to be one of [\"add_existing_table_or_view\","
        ));
        assert!(
            e.error
                .message
                .ends_with("\"set_custom_types\"], but found tag \"random\"")
        );
    }

    #[test]
    fn bad_version() {
        let e = Query::parse(
            &json!({"type": "track_function", "version": 3, "args": {}}),
            &Path::root(),
        )
        .unwrap_err();
        assert_eq!(e.error.message, "expected 1 or 2, encountered 3");
        assert_eq!(e.error.path, "$.version");
    }

    #[test]
    fn bulk_paths() {
        let q = json!({"type": "bulk", "args": [{"type": "select", "args": {"table": "a"}}]});
        let e = Query::parse(&q, &Path::root()).unwrap_err();
        assert_eq!(e.error.path, "$.args[0].args");
        assert_eq!(
            e.error.message,
            "parsing Hasura.RQL.DML.Types.SelectG(SelectG) failed, key \"columns\" not found"
        );
    }

    #[test]
    fn missing_args_names_constructor() {
        let e = Query::parse(&json!({"type": "run_sql"}), &Path::root()).unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing Hasura.Server.API.Query.RQLQueryV1(RQRunSql) failed, key \"args\" not found"
        );
    }

    #[test]
    fn admin_only() {
        let q = json!({"type": "delete", "args": {"table": "a", "where": {}}});
        let dml = Query::parse(&q, &Path::root()).unwrap();
        assert!(!dml.requires_admin());
        assert!(dml.writes_data());
        let sql = json!({"type": "run_sql", "args": {"sql": "select 1"}});
        assert!(Query::parse(&sql, &Path::root()).unwrap().requires_admin());
        let track = json!({"type": "track_table", "args": {"table": "a"}});
        assert!(
            Query::parse(&track, &Path::root())
                .unwrap()
                .requires_admin()
        );
    }

    #[test]
    fn dml_args_parse_up_front() {
        let e = Query::parse(&json!({"type": "insert", "args": {}}), &Path::root()).unwrap_err();
        assert_eq!(e.error.message, "key \"table\" not found");
        assert_eq!(e.error.path, "$.args");
        let q = json!({"type": "bulk", "args": [{"type": "update", "args": {"table": "a"}}]});
        let e = Query::parse(&q, &Path::root()).unwrap_err();
        assert_eq!(e.error.message, "key \"where\" not found");
        assert_eq!(e.error.path, "$.args[0].args");
    }

    #[test]
    fn run_sql_args_parse_up_front() {
        let e = Query::parse(&json!({"type": "run_sql", "args": {}}), &Path::root()).unwrap_err();
        assert_eq!(e.error.path, "$.args");
    }

    #[test]
    fn legacy_metadata_names() {
        let q = json!({"type": "create_select_permission", "args": {}});
        let Query::Metadata { command, .. } = Query::parse(&q, &Path::root()).unwrap() else {
            panic!("expected a metadata query");
        };
        assert_eq!(command.name, "pg_create_select_permission");
        let q = json!({"type": "add_existing_table_or_view", "args": {}});
        let Query::Metadata { command, .. } = Query::parse(&q, &Path::root()).unwrap() else {
            panic!("expected a metadata query");
        };
        assert_eq!(command.name, "pg_track_table");
        let q = json!({"type": "export_metadata", "args": {}});
        let Query::Metadata { command, .. } = Query::parse(&q, &Path::root()).unwrap() else {
            panic!("expected a metadata query");
        };
        assert_eq!(command.name, "export_metadata");
    }
}
