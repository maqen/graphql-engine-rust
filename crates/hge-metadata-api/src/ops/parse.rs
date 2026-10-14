//! Argument parsing with aeson's error wording and JSON paths.
//!
//! V2 parses command arguments with aeson; a failure is `400 parse-failed` whose message and
//! path depend on the parser (`withObject "Name"`, generic records, `.:`/`.:?`). [`Obj`]
//! reproduces the common cases so the errors match V2 exactly.

use hge_core::QualifiedTable;
use serde_json::{Map, Value};

use crate::api::json_kind;
use crate::response::{ApiError, ApiResult, parse_failed};

/// A JSON object being parsed at `path`. `ty` is the aeson type label used in "key not
/// found" messages of generic record parsers (`None` for hand-written `withObject` parsers,
/// which report a bare `key "x" not found`).
pub(crate) struct Obj<'a> {
    pub map: &'a Map<String, Value>,
    pub path: String,
    ty: Option<String>,
}

/// `parsing <ty> failed, expected Object, but encountered <kind>` for non-objects.
pub(crate) fn object<'a>(v: &'a Value, path: &str, ty: &str) -> ApiResult<Obj<'a>> {
    match v {
        Value::Object(map) => Ok(Obj {
            map,
            path: path.to_owned(),
            ty: None,
        }),
        other => Err(parse_failed(
            path,
            format!(
                "parsing {ty} failed, expected Object, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

/// Like [`object`], for a generically derived record: missing keys are reported as
/// `parsing <ty> failed, key "k" not found`.
pub(crate) fn record<'a>(v: &'a Value, path: &str, ty: &str) -> ApiResult<Obj<'a>> {
    let mut o = object(v, path, ty)?;
    o.ty = Some(ty.to_owned());
    Ok(o)
}

impl<'a> Obj<'a> {
    pub fn sub(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    pub fn missing(&self, key: &str) -> ApiError {
        let msg = match &self.ty {
            Some(ty) => format!("parsing {ty} failed, key \"{key}\" not found"),
            None => format!("key \"{key}\" not found"),
        };
        parse_failed(&self.path, msg)
    }

    /// The value at `key`, `None` when absent or `null` (aeson's `.:?`).
    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    /// The value at `key` (aeson's `.:`).
    pub fn req(&self, key: &str) -> ApiResult<&'a Value> {
        self.map.get(key).ok_or_else(|| self.missing(key))
    }

    pub fn string(&self, key: &str) -> ApiResult<String> {
        text(self.req(key)?, &self.sub(key))
    }

    pub fn opt_string(&self, key: &str) -> ApiResult<Option<String>> {
        self.get(key).map(|v| text(v, &self.sub(key))).transpose()
    }

    pub fn opt_bool(&self, key: &str) -> ApiResult<Option<bool>> {
        self.get(key)
            .map(|v| boolean(v, &self.sub(key)))
            .transpose()
    }

    pub fn bool_or(&self, key: &str, default: bool) -> ApiResult<bool> {
        Ok(self.opt_bool(key)?.unwrap_or(default))
    }

    pub fn req_bool(&self, key: &str) -> ApiResult<bool> {
        boolean(self.req(key)?, &self.sub(key))
    }

    /// A GraphQL name (`<x> is not valid GraphQL name`).
    pub fn name(&self, key: &str) -> ApiResult<String> {
        let s = self.string(key)?;
        graphql_name(&s, &self.sub(key))?;
        Ok(s)
    }

    pub fn opt_array(&self, key: &str) -> ApiResult<Option<&'a Vec<Value>>> {
        self.get(key).map(|v| array(v, &self.sub(key))).transpose()
    }

    pub fn array(&self, key: &str) -> ApiResult<&'a Vec<Value>> {
        array(self.req(key)?, &self.sub(key))
    }

    /// An optional integer (aeson's `Int` under `.:?`).
    pub fn opt_int(&self, key: &str) -> ApiResult<Option<i64>> {
        self.get(key).map(|v| int(v, &self.sub(key))).transpose()
    }
}

pub(crate) fn text(v: &Value, path: &str) -> ApiResult<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(parse_failed(
            path,
            format!(
                "parsing Text failed, expected String, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

pub(crate) fn boolean(v: &Value, path: &str) -> ApiResult<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(parse_failed(
            path,
            format!(
                "parsing Bool failed, expected Boolean, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

/// aeson's `Int`: integral numbers, including ones written with a fraction (`5.0`).
pub(crate) fn int(v: &Value, path: &str) -> ApiResult<i64> {
    match v {
        Value::Number(n) if n.as_i64().is_some() => Ok(n.as_i64().unwrap_or_default()),
        Value::Number(n)
            if n.as_f64().is_some_and(|f| {
                f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64
            }) =>
        {
            Ok(n.as_f64().unwrap_or_default() as i64)
        }
        Value::Number(n) => Err(parse_failed(
            path,
            format!(
                "parsing Int failed, value is either floating or will cause over or underflow {n}"
            ),
        )),
        other => Err(parse_failed(
            path,
            format!(
                "parsing Int failed, expected Number, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

pub(crate) fn array<'a>(v: &'a Value, path: &str) -> ApiResult<&'a Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a),
        other => Err(parse_failed(
            path,
            format!(
                "parsing [] failed, expected Array, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

pub(crate) fn is_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn graphql_name(s: &str, path: &str) -> ApiResult<()> {
    if is_graphql_name(s) {
        Ok(())
    } else {
        Err(parse_failed(path, format!("{s} is not valid GraphQL name")))
    }
}

/// A qualified table/function name in either JSON form, as `{"name", "schema"}`.
pub(crate) fn qualified(v: &Value, path: &str) -> ApiResult<QualifiedTable> {
    match v {
        Value::String(s) => Ok(QualifiedTable::public(s.clone())),
        Value::Object(o) => {
            let name = match o.get("name") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => {
                    return Err(parse_failed(
                        &format!("{path}.name"),
                        format!(
                            "parsing Text failed, expected String, but encountered {}",
                            json_kind(other)
                        ),
                    ));
                }
                None => return Err(parse_failed(path, "key \"name\" not found")),
            };
            let schema = match o.get("schema") {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => "public".to_owned(),
                Some(other) => {
                    return Err(parse_failed(
                        &format!("{path}.schema"),
                        format!(
                            "parsing Text failed, expected String, but encountered {}",
                            json_kind(other)
                        ),
                    ));
                }
            };
            Ok(QualifiedTable::new(schema, name))
        }
        other => Err(parse_failed(
            path,
            format!(
                "parsing QualifiedObject failed, expected Object, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

/// `{"name": …, "schema": …}` (V2's `QualifiedObject` encoding, keys sorted).
pub(crate) fn qualified_json(q: &QualifiedTable) -> Value {
    serde_json::json!({"name": q.name, "schema": q.schema})
}

/// V2's `dquoteList`: `"a", "b"`.
pub(crate) fn dquote_list<S: AsRef<str>>(items: &[S]) -> String {
    items
        .iter()
        .map(|s| format!("\"{}\"", s.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Values occurring more than once, in first-occurrence order.
pub(crate) fn duplicates<'s>(items: impl IntoIterator<Item = &'s str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut dups: Vec<String> = vec![];
    for i in items {
        if !seen.insert(i) && !dups.iter().any(|d| d == i) {
            dups.push(i.to_owned());
        }
    }
    dups
}

/// Parse a list of header configurations (`{name, value}` / `{name, value_from_env}`).
pub(crate) fn headers(v: &Value, path: &str) -> ApiResult<Vec<Value>> {
    let items = array(v, path)?;
    let mut out = Vec::with_capacity(items.len());
    for (i, h) in items.iter().enumerate() {
        let p = format!("{path}[{i}]");
        let o = object(h, &p, "HeaderConf")?;
        let name = o.string("name")?;
        let mut m = Map::new();
        m.insert("name".into(), Value::String(name));
        if let Some(v) = o.get("value") {
            m.insert("value".into(), Value::String(text(v, &o.sub("value"))?));
        } else if let Some(v) = o.get("value_from_env") {
            m.insert(
                "value_from_env".into(),
                Value::String(text(v, &o.sub("value_from_env"))?),
            );
        } else {
            return Err(parse_failed(
                &p,
                "expecting value or value_from_env keys".to_owned(),
            ));
        }
        out.push(Value::Object(m));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_names() {
        assert!(is_graphql_name("_a1"));
        assert!(!is_graphql_name("1a"));
        assert!(!is_graphql_name("a-b"));
        assert!(!is_graphql_name(""));
    }

    #[test]
    fn integral_floats_are_ints() {
        assert_eq!(int(&serde_json::json!(5.0), "$").unwrap(), 5);
        assert_eq!(int(&serde_json::json!(-3), "$").unwrap(), -3);
        let e = int(&serde_json::json!(5.5), "$.x").unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing Int failed, value is either floating or will cause over or underflow 5.5"
        );
    }
}
