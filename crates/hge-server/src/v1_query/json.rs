//! Decoding request JSON with V2's (aeson's) error messages and JSON paths.

use hge_core::{Code, QualifiedTable};

use hge_metadata_api::ApiError;
use serde_json::{Map, Value as Json};

/// A JSON path in V2's notation: `$.args.where`, `$.args["$set"]`, `$.args.columns[2]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    pub fn root() -> Self {
        Path("$".to_owned())
    }

    pub fn key(&self, key: &str) -> Path {
        let mut p = self.0.clone();
        let identifier = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if identifier {
            p.push('.');
            p.push_str(key);
        } else {
            p.push('[');
            p.push_str(&Json::String(key.to_owned()).to_string());
            p.push(']');
        }
        Path(p)
    }

    pub fn index(&self, i: usize) -> Path {
        Path(format!("{}[{i}]", self.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An error at `path` with HTTP status 400 (V2's `throw400`).
pub fn err400(code: Code, message: impl Into<String>, path: &Path) -> ApiError {
    ApiError::bad(code, message).with_path(path.as_str())
}

pub fn parse_failed(message: impl Into<String>, path: &Path) -> ApiError {
    err400(Code::ParseFailed, message, path)
}

/// aeson's name for a JSON value's type.
pub fn kind(v: &Json) -> &'static str {
    match v {
        Json::Null => "Null",
        Json::Bool(_) => "Boolean",
        Json::Number(_) => "Number",
        Json::String(_) => "String",
        Json::Array(_) => "Array",
        Json::Object(_) => "Object",
    }
}

/// `parsing <what> failed, expected <expected>, but encountered <kind>`
pub fn type_mismatch(what: &str, expected: &str, v: &Json, path: &Path) -> ApiError {
    parse_failed(
        format!(
            "parsing {what} failed, expected {expected}, but encountered {}",
            kind(v)
        ),
        path,
    )
}

/// A JSON object being decoded, with the path it lives at.
pub struct Obj<'a> {
    pub map: &'a Map<String, Json>,
    pub path: Path,
    /// aeson's record name for "parsing X failed, key ..." messages; `None` for V2's
    /// hand-written instances, which report a bare `key "k" not found`.
    record: Option<&'static str>,
}

impl<'a> Obj<'a> {
    pub fn new(v: &'a Json, path: Path, what: &str) -> Result<Self, ApiError> {
        match v {
            Json::Object(map) => Ok(Obj {
                map,
                path,
                record: None,
            }),
            other => Err(type_mismatch(what, "Object", other, &path)),
        }
    }

    /// A generically derived record: errors read `parsing <record> failed, …`.
    pub fn record(v: &'a Json, path: Path, record: &'static str) -> Result<Self, ApiError> {
        let mut obj = Obj::new(v, path, record)?;
        obj.record = Some(record);
        Ok(obj)
    }

    pub fn get(&self, key: &str) -> Option<&'a Json> {
        self.map.get(key)
    }

    /// An optional key; `null` counts as absent.
    pub fn opt(&self, key: &str) -> Option<&'a Json> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    pub fn req(&self, key: &str) -> Result<&'a Json, ApiError> {
        self.map.get(key).ok_or_else(|| {
            let msg = match self.record {
                Some(r) => format!("parsing {r} failed, key \"{key}\" not found"),
                None => format!("key \"{key}\" not found"),
            };
            parse_failed(msg, &self.path)
        })
    }

    pub fn at(&self, key: &str) -> Path {
        self.path.key(key)
    }

    pub fn string(&self, key: &str) -> Result<Option<String>, ApiError> {
        self.opt(key).map(|v| string(v, &self.at(key))).transpose()
    }

    pub fn int(&self, key: &str) -> Result<Option<i64>, ApiError> {
        self.opt(key).map(|v| int(v, &self.at(key))).transpose()
    }

    pub fn strings(&self, key: &str) -> Result<Option<Vec<String>>, ApiError> {
        self.opt(key).map(|v| strings(v, &self.at(key))).transpose()
    }
}

pub fn string(v: &Json, path: &Path) -> Result<String, ApiError> {
    match v {
        Json::String(s) => Ok(s.clone()),
        other => Err(type_mismatch("Text", "String", other, path)),
    }
}

pub fn int(v: &Json, path: &Path) -> Result<i64, ApiError> {
    match v {
        Json::Number(n) => n.as_i64().ok_or_else(|| {
            parse_failed(
                format!(
                    "parsing Int failed, value is either floating or will cause over or underflow {n}"
                ),
                path,
            )
        }),
        other => Err(type_mismatch("Int", "Number", other, path)),
    }
}

pub fn strings(v: &Json, path: &Path) -> Result<Vec<String>, ApiError> {
    match v {
        Json::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| string(item, &path.index(i)))
            .collect(),
        other => Err(type_mismatch("[]", "Array", other, path)),
    }
}

/// A table name: `"author"` or `{"schema": "s", "name": "author"}`.
pub fn table_name(v: &Json, path: &Path) -> Result<QualifiedTable, ApiError> {
    match v {
        Json::String(name) => Ok(QualifiedTable::public(name)),
        Json::Object(_) => {
            let o = Obj::new(v, path.clone(), "QualifiedObject")?;
            let name = string(o.req("name")?, &o.at("name"))?;
            let schema = o.string("schema")?.unwrap_or_else(|| "public".to_owned());
            Ok(QualifiedTable::new(schema, name))
        }
        other => Err(parse_failed(
            format!(
                "expecting a string or an object for a table name, but encountered {}",
                kind(other)
            ),
            path,
        )),
    }
}

/// A table as V2 prints it in messages: `author`, or `schema.author` outside `public`.
pub fn display_table(t: &QualifiedTable) -> String {
    if t.schema == "public" {
        t.name.clone()
    } else {
        format!("{}.{}", t.schema, t.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_like_v2() {
        let p = Path::root().key("args");
        assert_eq!(p.key("on_conflict").as_str(), "$.args.on_conflict");
        assert_eq!(p.key("$set").as_str(), r#"$.args["$set"]"#);
        assert_eq!(p.key("columns").index(2).as_str(), "$.args.columns[2]");
    }

    #[test]
    fn aeson_messages() {
        let v = serde_json::json!({"limit": "3"});
        let o = Obj::new(&v, Path::root().key("args"), "Object").unwrap();
        let e = o.int("limit").unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing Int failed, expected Number, but encountered String"
        );
        assert_eq!(e.error.path, "$.args.limit");
        let e = o.req("where").unwrap_err();
        assert_eq!(e.error.message, "key \"where\" not found");
        assert_eq!(e.error.path, "$.args");
    }
}
