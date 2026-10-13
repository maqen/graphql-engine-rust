//! Request variables of a REST endpoint (V2's `runCustomEndpoint` / `resolveVar`).

use hge_core::{Code, Error};
use serde_json::{Map, Value};

use crate::Endpoint;

/// Decoded `key=value` pairs of a query string, in order.
pub type QueryParams = Vec<(String, String)>;

/// Parse a raw query string (`a=1&b=x%20y`); `+` means space.
pub fn parse_query_string(query: &str) -> QueryParams {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

/// Percent-decode `s` (invalid escapes are kept as written); `plus_space` turns `+` into
/// a space, as in query strings.
pub(crate) fn percent_decode(s: &str, plus_space: bool) -> String {
    if !s.contains(['%', '+']) {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| {
                    std::str::from_utf8(h)
                        .ok()
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                });
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' if plus_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A request value: text from the URL or query string, or JSON from the body.
enum Param<'a> {
    Text(&'a str),
    Json(&'a Value),
}

fn is_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

impl Endpoint {
    /// The GraphQL variables of a request. Later sources win: query string, then the
    /// top-level keys of a JSON object body, then URL segments. Keys that are not GraphQL
    /// names are dropped; declared variables that are missing are left out.
    pub fn variables(
        &self,
        query: &[(String, String)],
        body: Option<&Map<String, Value>>,
        path: &[(String, String)],
    ) -> Result<Map<String, Value>, Error> {
        let mut params: Vec<(&str, Param<'_>)> = vec![];
        let all = query
            .iter()
            .map(|(k, v)| (k.as_str(), Param::Text(v)))
            .chain(
                body.into_iter()
                    .flatten()
                    .map(|(k, v)| (k.as_str(), Param::Json(v))),
            )
            .chain(path.iter().map(|(k, v)| (k.as_str(), Param::Text(v))));
        for (name, value) in all {
            if !is_graphql_name(name) {
                continue;
            }
            match params.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = value,
                None => params.push((name, value)),
            }
        }

        let mut out = Map::new();
        for (name, value) in params {
            let Some(def) = self.variables.iter().find(|d| d.name == name) else {
                return Err(Error::new(
                    Code::BadRequest,
                    format!("Unexpected variable {name}"),
                ));
            };
            let resolved = match value {
                Param::Json(v) => Some(v.clone()),
                Param::Text(text) => {
                    if def.list {
                        return Err(Error::new(
                            Code::BadRequest,
                            format!(
                                "List variables are not currently supported in URL or Query \
                                 parameters. (Variable {name}, with value \"{text}\")"
                            ),
                        ));
                    }
                    text_value(&def.type_name, def.nullable, text)
                }
            };
            if let Some(v) = resolved {
                out.insert(name.to_owned(), v);
            }
        }
        Ok(out)
    }
}

/// A URL or query-string value for a variable of named type `ty`; `None` leaves it unset.
fn text_value(ty: &str, nullable: bool, text: &str) -> Option<Value> {
    if text.is_empty() {
        if ty == "Boolean" {
            return Some(Value::Bool(true));
        }
        if nullable {
            return None;
        }
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Bool(_)) if matches!(ty, "Boolean" | "bool") => Some(v),
        Ok(v @ Value::Number(_))
            if matches!(
                ty,
                "Int" | "Float" | "Number" | "Double" | "float8" | "numeric"
            ) =>
        {
            Some(v)
        }
        _ => Some(Value::String(text.to_owned())),
    }
}
