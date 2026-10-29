//! `metadata apply`: the project's metadata files to one metadata JSON document, with the
//! official CLI's rules (which keys each file may hold, `!include` resolution, actions
//! merged from `actions.graphql`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use serde_yaml::Value as Yaml;

use crate::config::yaml_to_json;
use crate::error::{Error, Result};
use crate::logger::Logger;
use crate::metadata::sdl;

/// `errParsingMetadataObject`'s message.
pub fn parse_error(object: &str, file: &str, err: &str) -> Error {
    Error(format!(
        "error parsing metadata \nobject: {object}\nfile: {file}\nerror: {err}"
    ))
}

fn cannot_build(key: &str, e: Error) -> Error {
    Error(format!("cannot build {key} from project: {e}"))
}

/// Read a metadata file; `Ok(None)` when it does not exist.
fn read(dir: &Path, file: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(dir.join(file)) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error(e.to_string())),
    }
}

pub fn parse_yaml(text: &str) -> std::result::Result<Yaml, String> {
    if text.trim().is_empty() {
        return Ok(Yaml::Null);
    }
    serde_yaml::from_str(text).map_err(|e| format!("yaml: {e}"))
}

/// Build the metadata JSON for a config v3 project.
pub fn build(dir: &Path, logger: &Logger) -> Result<Value> {
    let mut parts: Vec<(&str, Value)> = Vec::new();
    let mut add = |key: &'static str, v: Option<Value>| {
        if let Some(v) = v {
            parts.push((key, v));
        }
    };

    // Objects in the order the CLI builds them; later ones win for the same key.
    let missing = |key: &str| {
        logger.debug(&format!(
            "metadata file for {key} was not found, assuming an empty file"
        ))
    };

    match read(dir, "version.yaml")? {
        None => missing("version"),
        Some(text) => {
            let y = parse_yaml(&text)
                .map_err(|e| cannot_build("version", parse_error("version", "version.yaml", &e)))?;
            let v = match y.get("version") {
                None | Some(Yaml::Null) => json!(0),
                Some(Yaml::Number(n)) if n.is_i64() => json!(n.as_i64()),
                Some(Yaml::String(s)) if s.parse::<i64>().is_ok() => {
                    json!(s.parse::<i64>().unwrap_or(0))
                }
                Some(other) => {
                    return Err(cannot_build(
                        "version",
                        parse_error(
                            "version",
                            "version.yaml",
                            &format!(
                                "yaml: unmarshal errors:\n  line 1: cannot unmarshal {} into int",
                                describe(other)
                            ),
                        ),
                    ));
                }
            };
            add("version", Some(v));
        }
    }

    match read(dir, "databases/databases.yaml")? {
        None => missing("sources"),
        Some(text) => add(
            "sources",
            Some(build_sources(dir, &text).map_err(|e| cannot_build("sources", e))?),
        ),
    }

    let raw_seq = |key: &'static str, file: &str| -> Result<Option<Value>> {
        match read(dir, file)? {
            None => {
                missing(key);
                Ok(None)
            }
            Some(text) => {
                let y =
                    parse_yaml(&text).map_err(|e| cannot_build(key, parse_error(key, file, &e)))?;
                match y {
                    Yaml::Null => Ok(Some(json!([]))),
                    Yaml::Sequence(_) => Ok(Some(yaml_to_json(&y))),
                    other => Err(cannot_build(
                        key,
                        parse_error(
                            key,
                            file,
                            &format!(
                                "yaml: unmarshal errors:\n  line 1: cannot unmarshal {} into []yaml.Node",
                                describe(&other)
                            ),
                        ),
                    )),
                }
            }
        }
    };

    add(
        "remote_schemas",
        raw_seq("remote_schemas", "remote_schemas.yaml")?,
    );
    add(
        "query_collections",
        raw_seq("query_collections", "query_collections.yaml")?,
    );
    add("allowlist", raw_seq("allowlist", "allow_list.yaml")?);

    if read(dir, "actions.yaml")?.is_none() && read(dir, "actions.graphql")?.is_none() {
        // Go fails reading actions.graphql first.
    }
    let (actions, custom_types) = build_actions(dir).map_err(|e| cannot_build("actions", e))?;
    add("actions", actions);
    add("custom_types", custom_types);

    add(
        "cron_triggers",
        raw_seq("cron_triggers", "cron_triggers.yaml")?,
    );

    match read(dir, "network.yaml")? {
        None => missing("network"),
        Some(text) => {
            let y = parse_yaml(&text)
                .map_err(|e| cannot_build("network", parse_error("network", "network.yaml", &e)))?;
            add("network", Some(pick(&y, &["tls_allowlist"])));
        }
    }

    match raw_seq("rest_endpoints", "rest_endpoints.yaml")? {
        Some(Value::Array(a)) if a.is_empty() => {}
        other => add("rest_endpoints", other),
    }
    add(
        "inherited_roles",
        raw_seq("inherited_roles", "inherited_roles.yaml")?,
    );

    for (key, file) in [
        ("opentelemetry", "opentelemetry.yaml"),
        ("backend_configs", "backend_configs.yaml"),
    ] {
        match read(dir, file)? {
            None => missing(key),
            Some(text) => {
                let y =
                    parse_yaml(&text).map_err(|e| cannot_build(key, parse_error(key, file, &e)))?;
                add(
                    if key == "opentelemetry" {
                        "opentelemetry"
                    } else {
                        "backend_configs"
                    },
                    Some(yaml_to_json(&y)),
                );
            }
        }
    }

    match read(dir, "api_limits.yaml")? {
        None => missing("api_limits"),
        Some(text) => {
            let y = parse_yaml(&text).map_err(|e| {
                cannot_build(
                    "api_limits",
                    parse_error("api_limits", "api_limits.yaml", &e),
                )
            })?;
            add(
                "api_limits",
                Some(pick(
                    &y,
                    &[
                        "disabled",
                        "rate_limit",
                        "depth_limit",
                        "node_limit",
                        "time_limit",
                        "batch_limit",
                    ],
                )),
            );
        }
    }

    match read(dir, "graphql_schema_introspection.yaml")? {
        None => missing("graphql_schema_introspection"),
        Some(text) => {
            let y = parse_yaml(&text).map_err(|e| {
                cannot_build(
                    "graphql_schema_introspection",
                    parse_error(
                        "graphql_schema_introspection",
                        "graphql_schema_introspection.yaml",
                        &e,
                    ),
                )
            })?;
            let roles = match y.get("disabled_for_roles") {
                Some(r @ Yaml::Sequence(_)) => yaml_to_json(r),
                _ => json!([]),
            };
            add(
                "graphql_schema_introspection",
                Some(json!({ "disabled_for_roles": roles })),
            );
        }
    }

    match read(dir, "metrics_config.yaml")? {
        None => missing("metrics_config"),
        Some(text) => {
            let y = parse_yaml(&text).map_err(|e| {
                cannot_build(
                    "metrics_config",
                    parse_error("metrics_config", "metrics_config.yaml", &e),
                )
            })?;
            if let Yaml::Mapping(m) = &y
                && !m.is_empty()
            {
                let mut entries: Vec<(String, Value)> = m
                    .iter()
                    .map(|(k, v)| (k.as_str().unwrap_or_default().to_string(), yaml_to_json(v)))
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                add(
                    "metrics_config",
                    Some(Value::Object(entries.into_iter().collect())),
                );
            }
        }
    }

    // The `Metadata` struct: fixed key order, `omitempty` everywhere but `version`.
    let order = [
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
    let mut out = Map::new();
    for key in order {
        let v = parts
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone());
        match v {
            // `omitempty` on an `any` field only drops a nil interface: files that are
            // present stay, even when empty.
            Some(v) if key == "version" || !v.is_null() => {
                out.insert(key.into(), v);
            }
            None if key == "version" => {
                out.insert(key.into(), Value::Null);
            }
            _ => {}
        }
    }
    Ok(Value::Object(out))
}

/// Keep only `keys` of a mapping (a typed Go struct of `yaml.Node` fields, `omitempty`).
fn pick(y: &Yaml, keys: &[&str]) -> Value {
    let mut out = Map::new();
    if let Yaml::Mapping(m) = y {
        for key in keys {
            if let Some(v) = m.get(*key) {
                out.insert((*key).into(), yaml_to_json(v));
            }
        }
    }
    Value::Object(out)
}

fn describe(y: &Yaml) -> &'static str {
    match y {
        Yaml::Null => "!!null",
        Yaml::Bool(_) => "!!bool",
        Yaml::Number(n) if n.is_f64() => "!!float",
        Yaml::Number(_) => "!!int",
        Yaml::String(_) => "!!str",
        Yaml::Sequence(_) => "!!seq",
        Yaml::Mapping(_) => "!!map",
        Yaml::Tagged(_) => "!!str",
    }
}

const SOURCE_KEYS: &[&str] = &[
    "name",
    "kind",
    "configuration",
    "query_tags",
    "customization",
    "health_check",
    "logical_models",
    "native_queries",
    "stored_procedures",
    "tables",
    "functions",
];

fn build_sources(dir: &Path, text: &str) -> Result<Value> {
    let err = |e: &str| parse_error("sources", "databases.yaml", e);
    let y = parse_yaml(text).map_err(|e| err(&format!("parsing error: {e}")))?;
    let items = match y {
        Yaml::Null => vec![],
        Yaml::Sequence(items) => items,
        other => {
            return Err(err(&format!(
                "parsing error: yaml: unmarshal errors:\n  line 1: cannot unmarshal {} into []sources.Source",
                describe(&other)
            )));
        }
    };
    let base = dir.join("databases");
    let mut out = Vec::new();
    for source in items {
        let Yaml::Mapping(m) = &source else {
            return Err(err(
                "parsing error: yaml: unmarshal errors: source is not a mapping",
            ));
        };
        let mut s = Map::new();
        for key in SOURCE_KEYS {
            let v = m.get(*key);
            let v = match (*key, v) {
                ("tables" | "functions", Some(v)) => {
                    let resolved = resolve_includes(v, &base).map_err(|e| err(&e))?;
                    Some(yaml_to_json(&resolved))
                }
                (_, v) => v.map(yaml_to_json),
            };
            match (*key, v) {
                ("name" | "kind", v) => {
                    let s_val = match v {
                        Some(Value::String(x)) => x,
                        Some(Value::Null) | None => String::new(),
                        Some(other) => other.to_string(),
                    };
                    s.insert((*key).into(), Value::String(s_val));
                }
                ("configuration" | "tables", v) => {
                    s.insert((*key).into(), v.unwrap_or(Value::Null));
                }
                (_, Some(v)) => {
                    s.insert((*key).into(), v);
                }
                (_, None) => {}
            }
        }
        out.push(Value::Object(s));
    }
    Ok(Value::Array(out))
}

/// Replace `!include <file>` (a tag, or any string containing `!include`) with the
/// file's contents, recursively; non-YAML files become strings.
pub fn resolve_includes(y: &Yaml, base: &Path) -> std::result::Result<Yaml, String> {
    resolve_includes_tracking(y, base, &mut Vec::new())
}

pub fn resolve_includes_tracking(
    y: &Yaml,
    base: &Path,
    files: &mut Vec<PathBuf>,
) -> std::result::Result<Yaml, String> {
    match y {
        Yaml::Sequence(items) => Ok(Yaml::Sequence(
            items
                .iter()
                .map(|i| resolve_includes_tracking(i, base, files))
                .collect::<std::result::Result<_, _>>()?,
        )),
        Yaml::Mapping(m) => {
            let mut out = serde_yaml::Mapping::new();
            for (k, v) in m {
                out.insert(k.clone(), resolve_includes_tracking(v, base, files)?);
            }
            Ok(Yaml::Mapping(out))
        }
        Yaml::Tagged(t) if t.tag == "!include" => {
            let file = match &t.value {
                Yaml::String(s) => s.clone(),
                other => serde_yaml::to_string(other)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            };
            include_file(&file, "!include", base, files)
        }
        Yaml::String(s) if s.contains("!include") => {
            let parts: Vec<&str> = s.split(' ').collect();
            let file = parts[1..].join(" ").trim_matches('"').to_string();
            include_file(&file, "!include", base, files)
        }
        other => Ok(other.clone()),
    }
}

fn include_file(
    file: &str,
    tag: &str,
    base: &Path,
    files: &mut Vec<PathBuf>,
) -> std::result::Result<Yaml, String> {
    const MAX_INCLUDE_DEPTH: usize = 64;
    thread_local! {
        static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    let location = base.join(file);
    let depth = DEPTH.with(|d| d.get());
    if depth >= MAX_INCLUDE_DEPTH {
        return Err(format!(
            "yaml tag resolver error:\ntag: {tag}\nfile: {}\nerror: !include nested more than {MAX_INCLUDE_DEPTH} levels deep (cyclic include?)",
            location.display()
        ));
    }
    let content = std::fs::read_to_string(&location).map_err(|e| {
        format!(
            "yaml tag resolver error:\ntag: {tag}\nfile: {}\nerror: open {}: {}",
            location.display(),
            location.display(),
            crate::context::io_message(&e)
        )
    })?;
    files.push(location.clone());
    if location.extension().and_then(|e| e.to_str()) != Some("yaml") {
        return Ok(Yaml::String(content));
    }
    let parsed = parse_yaml(&content).map_err(|e| {
        format!(
            "yaml tag resolver error:\ntag: {tag}\nfile: {}\nerror: {e}",
            location.display()
        )
    })?;
    let next_base = location.parent().map(Path::to_path_buf).unwrap_or_default();
    DEPTH.with(|d| d.set(depth + 1));
    let r = resolve_includes_tracking(&parsed, &next_base, files);
    DEPTH.with(|d| d.set(depth));
    r
}

// --- actions

fn build_actions(dir: &Path) -> Result<(Option<Value>, Option<Value>)> {
    let err = |e: &str| parse_error("actions", "actions.yaml", e);
    let graphql = std::fs::read_to_string(dir.join("actions.graphql")).map_err(|e| {
        err(&format!(
            "error in reading actions.graphql file: {}",
            if e.kind() == std::io::ErrorKind::NotFound {
                "metadata file not found".to_string()
            } else {
                e.to_string()
            }
        ))
    })?;
    let mut from = sdl::sdl_to_metadata(&graphql).map_err(|e| {
        err(&format!(
            "error in converting sdl to metadata: exit status 1: {e}"
        ))
    })?;
    let yaml_text = std::fs::read_to_string(dir.join("actions.yaml")).map_err(|e| {
        err(&format!(
            "error in reading actions.yaml: {}",
            if e.kind() == std::io::ErrorKind::NotFound {
                "metadata file not found".to_string()
            } else {
                e.to_string()
            }
        ))
    })?;
    let old =
        parse_yaml(&yaml_text).map_err(|e| err(&format!("error in reading actions.yaml: {e}")))?;

    // Actions: keep the SDL signature, take everything else from actions.yaml.
    let mut actions: Vec<Map<String, Value>> = from
        .actions
        .drain(..)
        .map(|a| {
            let name = a["name"].clone();
            let d = &a["definition"];
            let mut def = Map::new();
            def.insert("kind".into(), json!(""));
            def.insert("type".into(), d["type"].clone());
            def.insert("handler".into(), json!(""));
            def.insert("arguments".into(), d["arguments"].clone());
            def.insert("output_type".into(), d["output_type"].clone());
            let mut m = Map::new();
            m.insert("name".into(), name);
            m.insert("definition".into(), Value::Object(def));
            m
        })
        .collect();
    if let Some(Yaml::Sequence(old_actions)) = old.get("actions") {
        for old_action in old_actions {
            let name = old_action
                .get("name")
                .and_then(Yaml::as_str)
                .unwrap_or_default();
            let Some(new) = actions.iter_mut().find(|a| a["name"] == json!(name)) else {
                return Err(err(&format!(
                    "action {name} is not present in actions.graphql"
                )));
            };
            if let Some(p) = old_action.get("permissions") {
                new.insert("permissions".into(), yaml_to_json(p));
            }
            if let Some(c) = old_action.get("comment").filter(|c| !c.is_null()) {
                new.insert("comment".into(), yaml_to_json(c));
            }
            let od = old_action.get("definition");
            let field = |k: &str| od.and_then(|d| d.get(k));
            if let Some(Value::Object(def)) = new.get_mut("definition") {
                let s = |v: Option<&Yaml>| match v {
                    Some(Yaml::String(s)) => json!(s),
                    Some(Yaml::Null) | None => json!(""),
                    Some(other) => json!(serde_yaml::to_string(other).unwrap_or_default().trim()),
                };
                def.insert("kind".into(), s(field("kind")));
                def.insert("handler".into(), s(field("handler")));
                if let Some(Yaml::Bool(true)) = field("forward_client_headers") {
                    def.insert("forward_client_headers".into(), json!(true));
                }
                if let Some(h) = field("headers") {
                    def.insert("headers".into(), yaml_to_json(h));
                }
                if let Some(t) = field("timeout").and_then(Yaml::as_i64).filter(|t| *t != 0) {
                    def.insert("timeout".into(), json!(t));
                }
                for k in ["request_transform", "response_transform"] {
                    if let Some(v) = field(k) {
                        def.insert(k.into(), yaml_to_json(v));
                    }
                }
            }
        }
    }
    // The `ActionDef` struct: field order and `omitempty`.
    let actions: Vec<Value> = actions
        .into_iter()
        .map(|mut a| {
            if let Some(Value::Object(def)) = a.remove("definition") {
                let mut d = Map::new();
                for k in [
                    "kind",
                    "type",
                    "handler",
                    "arguments",
                    "output_type",
                    "forward_client_headers",
                    "headers",
                    "timeout",
                    "request_transform",
                    "response_transform",
                ] {
                    if let Some(v) = def.get(k) {
                        let keep = matches!(k, "kind" | "handler")
                            || !matches!(v, Value::String(s) if s.is_empty())
                                && !matches!(v, Value::Array(x) if x.is_empty())
                                && !v.is_null();
                        if keep {
                            d.insert(k.into(), v.clone());
                        }
                    }
                }
                let mut out = Map::new();
                out.insert("name".into(), a.remove("name").unwrap_or(Value::Null));
                out.insert("definition".into(), Value::Object(d));
                if let Some(p) = a.remove("permissions") {
                    out.insert("permissions".into(), p);
                }
                if let Some(c) = a.remove("comment") {
                    out.insert("comment".into(), c);
                }
                Value::Object(out)
            } else {
                Value::Object(a)
            }
        })
        .collect();

    // Custom types: SDL fields/values, description and relationships from actions.yaml.
    let mut kinds = [
        ("enums", std::mem::take(&mut from.enums)),
        ("input_objects", std::mem::take(&mut from.input_objects)),
        ("objects", std::mem::take(&mut from.objects)),
        ("scalars", std::mem::take(&mut from.scalars)),
    ];
    for (kind, defs) in kinds.iter_mut() {
        if let Some(Yaml::Sequence(old_defs)) = old.get("custom_types").and_then(|c| c.get(*kind)) {
            for old_def in old_defs {
                let name = old_def
                    .get("name")
                    .and_then(Yaml::as_str)
                    .unwrap_or_default();
                let Some(new) = defs.iter_mut().find(|d| d["name"] == json!(name)) else {
                    return Err(err(&format!(
                        "custom type {name} is not present in actions.graphql"
                    )));
                };
                let obj = new.as_object_mut().expect("custom type is an object");
                match old_def.get("description") {
                    Some(Yaml::Null) | None => {
                        obj.insert("description".into(), Value::Null);
                    }
                    Some(d) => {
                        obj.insert("description".into(), yaml_to_json(d));
                    }
                }
                match old_def.get("relationships") {
                    Some(r) => {
                        obj.insert("relationships".into(), yaml_to_json(r));
                    }
                    None => {
                        obj.remove("relationships");
                    }
                }
            }
        }
    }
    let total: usize = kinds.iter().map(|(_, d)| d.len()).sum();
    let custom_types = (total != 0).then(|| {
        let mut ct = Map::new();
        for (kind, defs) in kinds {
            let defs: Vec<Value> = defs.into_iter().map(custom_type_def).collect();
            ct.insert(kind.into(), Value::Array(defs));
        }
        Value::Object(ct)
    });
    Ok((
        (!actions.is_empty()).then_some(Value::Array(actions)),
        custom_types,
    ))
}

/// The `CustomTypeDef` struct: name, description (when set), fields, values, relationships.
fn custom_type_def(v: Value) -> Value {
    let Value::Object(m) = v else { return v };
    let mut out = Map::new();
    out.insert("name".into(), m.get("name").cloned().unwrap_or(Value::Null));
    if let Some(d) = m.get("description").filter(|d| !d.is_null()) {
        out.insert("description".into(), d.clone());
    }
    for k in ["fields", "values"] {
        if let Some(Value::Array(a)) = m.get(k)
            && !a.is_empty()
        {
            out.insert(k.into(), Value::Array(a.clone()));
        }
    }
    if let Some(r) = m.get("relationships") {
        out.insert("relationships".into(), r.clone());
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyclic_include_is_an_error() {
        let dir = std::env::temp_dir().join(format!("hge-cli-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.yaml"), "x: !include a.yaml\n").unwrap();
        let root = parse_yaml("- !include a.yaml\n").unwrap();
        let err = resolve_includes_tracking(&root, &dir, &mut Vec::new()).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(err.contains("cyclic include"), "{err}");
    }
}
