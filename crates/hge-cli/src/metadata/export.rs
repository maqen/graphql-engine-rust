//! `metadata export`: server metadata to the project's metadata files, file by file as
//! the official CLI's metadata objects write them.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::json::Json;
use crate::metadata::gqlfmt;
use crate::metadata::sdl;
use crate::yaml::Node;
use crate::yaml::emitter::ScalarStyle;

/// Relative path (from the metadata directory) to file contents.
pub type Files = BTreeMap<String, Vec<u8>>;

/// The top-level keys `JSONToYAML` keeps (its `metadata` struct), in order.
pub const METADATA_KEYS: &[&str] = &[
    "version",
    "sources",
    "tables",
    "functions",
    "actions",
    "custom_types",
    "remote_schemas",
    "query_collections",
    "allowlist",
    "cron_triggers",
    "network",
    "api_limits",
    "rest_endpoints",
    "inherited_roles",
    "opentelemetry",
    "backend_configs",
    "graphql_schema_introspection",
    "metrics_config",
];

/// `metadatautil.JSONToYAML` as a node: unknown top-level keys are dropped and the rest
/// follow the `metadata` struct order (`version` is always present).
pub fn metadata_node(server: &Json) -> Node {
    let full = Node::from_json(server);
    let mut fields = Vec::new();
    for key in METADATA_KEYS {
        match full.get(key) {
            Some(v) => fields.push((Node::string(*key), v.clone())),
            None if *key == "version" => fields.push((Node::string("version"), Node::raw("null"))),
            None => {}
        }
    }
    Node::Map(fields)
}

/// Export all metadata objects for a config v3 project.
pub fn export(server: &Json) -> Result<Files> {
    let md = metadata_node(server);
    let mut files = Files::new();
    // version
    if let Some(v) = md.get("version") {
        files.insert(
            "version.yaml".into(),
            yaml(&Node::Map(vec![(Node::string("version"), v.clone())])),
        );
    }
    export_sources(&md, &mut files)?;
    files.insert(
        "remote_schemas.yaml".into(),
        yaml(&export_remote_schemas(md.get("remote_schemas"))),
    );
    files.insert(
        "query_collections.yaml".into(),
        yaml(&export_query_collections(md.get("query_collections"))),
    );
    files.insert(
        "allow_list.yaml".into(),
        yaml(&default_seq(md.get("allowlist"))),
    );
    export_actions(&md, &mut files)?;
    files.insert(
        "cron_triggers.yaml".into(),
        yaml(&default_seq(md.get("cron_triggers"))),
    );
    files.insert("network.yaml".into(), yaml(&default_map(md.get("network"))));
    files.insert(
        "rest_endpoints.yaml".into(),
        yaml(&default_seq(md.get("rest_endpoints"))),
    );
    files.insert(
        "inherited_roles.yaml".into(),
        yaml(&default_seq(md.get("inherited_roles"))),
    );
    files.insert(
        "opentelemetry.yaml".into(),
        yaml(&default_map(md.get("opentelemetry"))),
    );
    files.insert(
        "backend_configs.yaml".into(),
        yaml(&default_map(md.get("backend_configs"))),
    );
    files.insert(
        "api_limits.yaml".into(),
        yaml(&default_map(md.get("api_limits"))),
    );
    let introspection = Node::Map(vec![(
        Node::string("disabled_for_roles"),
        md.get("graphql_schema_introspection")
            .and_then(|n| n.get("disabled_for_roles"))
            .filter(|n| matches!(n, Node::Seq(_)))
            .cloned()
            .unwrap_or(Node::Seq(vec![])),
    )]);
    files.insert(
        "graphql_schema_introspection.yaml".into(),
        yaml(&introspection),
    );
    files.insert(
        "metrics_config.yaml".into(),
        yaml(&default_map(md.get("metrics_config"))),
    );
    Ok(files)
}

fn yaml(n: &Node) -> Vec<u8> {
    n.to_yaml().into_bytes()
}

fn default_seq(n: Option<&Node>) -> Node {
    n.cloned().unwrap_or(Node::Seq(vec![]))
}

fn default_map(n: Option<&Node>) -> Node {
    n.cloned().unwrap_or(Node::Map(vec![]))
}

/// A Go `string` field decoded from a node.
pub fn go_string(n: Option<&Node>) -> String {
    match n {
        Some(Node::Scalar(s)) if s.is_str || s.value != "null" => s.value.clone(),
        _ => String::new(),
    }
}

/// A node re-encoded after a round trip through Go's `any`: strings re-quoted by
/// go-yaml's rules, maps with sorted keys. `None` for null.
pub fn go_any(n: &Node) -> Option<Node> {
    match n {
        Node::Scalar(s) if s.is_str => Some(Node::string(s.value.clone())),
        Node::Scalar(s) if s.value == "null" || s.value == "~" => None,
        Node::Scalar(s) => Some(Node::raw(go_number(&s.value))),
        Node::Seq(items) => Some(Node::Seq(
            items
                .iter()
                .map(|i| go_any(i).unwrap_or_else(|| Node::raw("null")))
                .collect(),
        )),
        Node::Map(fields) => {
            let mut entries: Vec<(String, Node)> = fields
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().unwrap_or_default().to_string(),
                        go_any(v).unwrap_or_else(|| Node::raw("null")),
                    )
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Some(Node::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| (Node::string(k), v))
                    .collect(),
            ))
        }
    }
}

/// A YAML number decoded into a Go `int` or `float64` and encoded again.
fn go_number(v: &str) -> String {
    if v == "true" || v == "false" {
        return v.to_string();
    }
    if let Ok(i) = v.parse::<i64>() {
        return i.to_string();
    }
    match v.parse::<f64>() {
        Ok(f) => go_float(f),
        Err(_) => v.to_string(),
    }
}

/// go-yaml's float encoding: `strconv.FormatFloat(f, 'g', -1, 64)` with `.inf`/`.nan`.
pub fn go_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            ".inf".into()
        } else {
            "-.inf".into()
        };
    }
    let s = format!("{f:e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..6).contains(&exp) {
        format!("{f}")
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", exp.abs())
    }
}

/// Builds a Go struct encoding: fields in declaration order with `omitempty` rules.
struct Struct(Vec<(Node, Node)>);

impl Struct {
    fn new() -> Self {
        Struct(Vec::new())
    }

    /// A `yaml.Node` field: omitted when absent if `omitempty`, `null` otherwise.
    fn node(&mut self, key: &str, v: Option<&Node>, omitempty: bool) -> &mut Self {
        match v {
            Some(v) => self.0.push((Node::string(key), v.clone())),
            None if !omitempty => self.0.push((Node::string(key), Node::raw("null"))),
            None => {}
        }
        self
    }

    /// A Go `string` field.
    fn string(&mut self, key: &str, v: &str, omitempty: bool) -> &mut Self {
        if !(omitempty && v.is_empty()) {
            self.0.push((Node::string(key), Node::string(v)));
        }
        self
    }

    fn raw(&mut self, key: &str, v: Node) -> &mut Self {
        self.0.push((Node::string(key), v));
        self
    }

    fn build(self) -> Node {
        Node::Map(self.0)
    }
}

fn seq_items(n: Option<&Node>) -> Vec<&Node> {
    match n {
        Some(Node::Seq(items)) => items.iter().collect(),
        _ => Vec::new(),
    }
}

fn include(path: &str) -> Node {
    Node::styled(format!("!include {path}"), ScalarStyle::DoubleQuoted)
}

fn export_sources(md: &Node, files: &mut Files) -> Result<()> {
    let Some(sources) = md.get("sources") else {
        return Ok(());
    };
    let Node::Seq(items) = sources else {
        return Err(Error(
            "cannot export sources from metadata: sources is not a list".into(),
        ));
    };
    let mut out = Vec::new();
    for source in items {
        let name = go_string(source.get("name"));
        let mut s = Struct::new();
        s.string("name", &name, false)
            .string("kind", &go_string(source.get("kind")), false)
            .node("configuration", source.get("configuration"), false)
            .node("query_tags", source.get("query_tags"), true)
            .node("customization", source.get("customization"), true)
            .node("health_check", source.get("health_check"), true)
            .node("logical_models", source.get("logical_models"), true)
            .node("native_queries", source.get("native_queries"), true)
            .node("stored_procedures", source.get("stored_procedures"), true);
        match source.get("tables") {
            Some(tables) => {
                let mut tags = Vec::new();
                for table in seq_items(Some(tables)) {
                    let file = table_file_name(table.get("table"))?;
                    tags.push(include(&file));
                    files.insert(format!("databases/{name}/tables/{file}"), yaml(table));
                }
                files.insert(
                    format!("databases/{name}/tables/tables.yaml"),
                    yaml(&Node::Seq(tags)),
                );
                s.raw("tables", include(&format!("{name}/tables/tables.yaml")));
            }
            None => {
                s.raw("tables", Node::raw("null"));
            }
        }
        if let Some(functions) = source.get("functions") {
            let fns = seq_items(Some(functions));
            if fns.is_empty() {
                s.raw("functions", functions.clone());
            } else {
                let mut tags = Vec::new();
                for f in fns {
                    let file = function_file_name(f.get("function"));
                    tags.push(include(&file));
                    files.insert(format!("databases/{name}/functions/{file}"), yaml(f));
                }
                files.insert(
                    format!("databases/{name}/functions/functions.yaml"),
                    yaml(&Node::Seq(tags)),
                );
                s.raw(
                    "functions",
                    include(&format!("{name}/functions/functions.yaml")),
                );
            }
        }
        out.push(s.build());
    }
    files.insert("databases/databases.yaml".into(), yaml(&Node::Seq(out)));
    Ok(())
}

fn table_file_name(table: Option<&Node>) -> Result<String> {
    match table {
        Some(Node::Seq(parts)) => Ok(format!(
            "{}.yaml",
            parts
                .iter()
                .map(|p| go_string(Some(p)))
                .collect::<Vec<_>>()
                .join("_")
        )),
        Some(t @ Node::Map(_)) => {
            let name = go_string(t.get("name"));
            let schema = go_string(t.get("schema"));
            if schema.is_empty() {
                Ok(format!("{}_{name}.yaml", go_string(t.get("dataset"))))
            } else {
                Ok(format!("{schema}_{name}.yaml"))
            }
        }
        None => Ok("_.yaml".into()),
        Some(Node::Scalar(s)) => Err(Error(format!(
            "cannot export sources from metadata: yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str `{}` into struct {{ Name string \"yaml:\\\"name\\\"\"; Schema string \"yaml:\\\"schema\\\"\"; Dataset string \"yaml:\\\"dataset\\\"\" }}",
            s.value
        ))),
    }
}

fn function_file_name(f: Option<&Node>) -> String {
    match f {
        Some(m @ Node::Map(_)) => {
            format!(
                "{}_{}.yaml",
                go_string(m.get("schema")),
                go_string(m.get("name"))
            )
        }
        Some(Node::Seq(parts)) => {
            format!(
                "{}.yaml",
                parts
                    .iter()
                    .map(|p| go_string(Some(p)))
                    .collect::<Vec<_>>()
                    .join("_")
            )
        }
        _ => ".yaml".into(),
    }
}

fn export_remote_schemas(n: Option<&Node>) -> Node {
    let Some(n) = n else {
        return Node::Seq(vec![]);
    };
    let mut out = Vec::new();
    for rs in seq_items(Some(n)) {
        let mut s = Struct::new();
        if let Some(name) = rs.get("name").and_then(go_any) {
            s.raw("name", name);
        }
        s.node("definition", rs.get("definition"), true)
            .node("comment", rs.get("comment"), true);
        let mut perms = Vec::new();
        for p in seq_items(rs.get("permissions")) {
            let mut ps = Struct::new();
            if let Some(role) = p.get("role").and_then(go_any) {
                ps.raw("role", role);
            }
            let mut schema = go_string(p.get("definition").and_then(|d| d.get("schema")));
            if let Some(formatted) = gqlfmt::format_schema(&schema).filter(|f| !f.is_empty()) {
                schema = formatted;
            }
            if !schema.is_empty() {
                ps.raw(
                    "definition",
                    Node::Map(vec![(Node::string("schema"), Node::string(schema))]),
                );
            }
            perms.push(ps.build());
        }
        if !perms.is_empty() {
            s.raw("permissions", Node::Seq(perms));
        }
        s.node("remote_relationships", rs.get("remote_relationships"), true);
        out.push(s.build());
    }
    Node::Seq(out)
}

fn export_query_collections(n: Option<&Node>) -> Node {
    let Some(n) = n else {
        return Node::Seq(vec![]);
    };
    let mut out = Vec::new();
    for c in seq_items(Some(n)) {
        let mut s = Struct::new();
        s.node("name", c.get("name"), true);
        let mut queries = Vec::new();
        for q in seq_items(c.get("definition").and_then(|d| d.get("queries"))) {
            let mut qs = Struct::new();
            qs.node("name", q.get("name"), true);
            let mut query = go_string(q.get("query"));
            if let Some(formatted) = gqlfmt::format_query(&query).filter(|f| !f.is_empty()) {
                query = formatted;
            }
            qs.string("query", &query, true);
            queries.push(qs.build());
        }
        s.raw(
            "definition",
            Node::Map(vec![(Node::string("queries"), Node::Seq(queries))]),
        );
        out.push(s.build());
    }
    Node::Seq(out)
}

fn export_actions(md: &Node, files: &mut Files) -> Result<()> {
    let actions = seq_items(md.get("actions"));
    let custom_types = md.get("custom_types");
    let graphql = sdl::metadata_to_sdl(&actions, custom_types);

    let mut out_actions = Vec::new();
    for a in &actions {
        let def = a.get("definition");
        let mut d = Struct::new();
        d.string("kind", &go_string(def.and_then(|d| d.get("kind"))), false)
            .string(
                "handler",
                &go_string(def.and_then(|d| d.get("handler"))),
                false,
            );
        if def
            .and_then(|d| d.get("forward_client_headers"))
            .and_then(Node::as_str)
            == Some("true")
        {
            d.raw("forward_client_headers", Node::bool(true));
        }
        d.node("headers", def.and_then(|d| d.get("headers")), true);
        let timeout = go_string(def.and_then(|d| d.get("timeout")));
        if let Ok(t) = timeout.parse::<i64>()
            && t != 0
        {
            d.raw("timeout", Node::raw(t.to_string()));
        }
        d.node(
            "request_transform",
            def.and_then(|d| d.get("request_transform")),
            true,
        )
        .node(
            "response_transform",
            def.and_then(|d| d.get("response_transform")),
            true,
        );
        let mut s = Struct::new();
        s.string("name", &go_string(a.get("name")), false)
            .raw("definition", d.build());
        s.node("permissions", a.get("permissions"), true);
        if let Some(c) = a.get("comment").and_then(go_any) {
            s.raw("comment", c);
        }
        out_actions.push(s.build());
    }
    let mut ct = Struct::new();
    for kind in ["enums", "input_objects", "objects", "scalars"] {
        let mut defs = Vec::new();
        for t in seq_items(custom_types.and_then(|c| c.get(kind))) {
            let mut s = Struct::new();
            s.string("name", &go_string(t.get("name")), false);
            if let Some(Node::Scalar(d)) = t.get("description")
                && (d.is_str || d.value != "null")
            {
                s.raw("description", Node::string(d.value.clone()));
            }
            if let Some(Node::Seq(values)) = t.get("values")
                && !values.is_empty()
            {
                s.raw("values", Node::Seq(values.clone()));
            }
            s.node("relationships", t.get("relationships"), true);
            defs.push(s.build());
        }
        ct.raw(kind, Node::Seq(defs));
    }
    let common = Node::Map(vec![
        (Node::string("actions"), Node::Seq(out_actions)),
        (Node::string("custom_types"), ct.build()),
    ]);
    files.insert("actions.yaml".into(), yaml(&common));
    files.insert("actions.graphql".into(), graphql.into_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn go_floats() {
        assert_eq!(super::go_float(1.5), "1.5");
        assert_eq!(super::go_float(1e21), "1e+21");
        assert_eq!(super::go_float(1234567.0), "1.234567e+06");
        assert_eq!(super::go_float(100000.0), "100000");
        assert_eq!(super::go_float(0.0001), "0.0001");
        assert_eq!(super::go_float(0.00001), "1e-05");
    }
}
