//! The JSONPath subset V2 accepts for `claims_namespace_path` and `claims_map`:
//! `$`, `.key`, `['key']`, `["key"]` and `[index]`.

use std::fmt;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Element {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath {
    pub elements: Vec<Element>,
    source: String,
}

impl JsonPath {
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut rest = source.strip_prefix('$').unwrap_or(source);
        let mut elements = vec![];
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix(".[") {
                // `$.['key']`, which V2 (and Auth0's docs) accept.
                let (element, r) = parse_bracket(r)?;
                elements.push(element);
                rest = r;
            } else if let Some(r) = rest.strip_prefix('.') {
                let end = r.find(['.', '[']).unwrap_or(r.len());
                if end == 0 {
                    return Err(format!("invalid property name at: {r}"));
                }
                elements.push(Element::Key(r[..end].to_owned()));
                rest = &r[end..];
            } else if let Some(r) = rest.strip_prefix('[') {
                let (element, r) = parse_bracket(r)?;
                elements.push(element);
                rest = r;
            } else if elements.is_empty() && rest.len() == source.len() {
                // A leading key without `$.`, e.g. `hasura.claims`.
                let end = rest.find(['.', '[']).unwrap_or(rest.len());
                elements.push(Element::Key(rest[..end].to_owned()));
                rest = &rest[end..];
            } else {
                return Err(format!("unexpected input: {rest}"));
            }
        }
        Ok(JsonPath {
            elements,
            source: source.to_owned(),
        })
    }

    /// The value at this path, if every step exists.
    pub fn get<'a>(&self, mut value: &'a Value) -> Option<&'a Value> {
        for e in &self.elements {
            value = match (e, value) {
                (Element::Key(k), Value::Object(o)) => o.get(k)?,
                (Element::Index(i), Value::Array(a)) => a.get(*i)?,
                _ => return None,
            };
        }
        Some(value)
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

fn parse_bracket(r: &str) -> Result<(Element, &str), String> {
    if let Some(quote) = r.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let body = &r[1..];
        let end = body
            .find(quote)
            .ok_or_else(|| format!("unterminated string at: {r}"))?;
        let rest = body[end + 1..]
            .strip_prefix(']')
            .ok_or_else(|| format!("expected ']' at: {}", &body[end + 1..]))?;
        Ok((Element::Key(body[..end].to_owned()), rest))
    } else {
        let end = r.find(']').ok_or_else(|| format!("expected ']' at: {r}"))?;
        let index = r[..end]
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("invalid array index: {}", &r[..end]))?;
        Ok((Element::Index(index), &r[end + 1..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_and_get() {
        let v = json!({"hasura": {"claims": {"x": 1}}, "roles": ["a", "b"], "a.b": {"c": 2}});
        let get = |p: &str| JsonPath::parse(p).unwrap().get(&v).cloned();
        assert_eq!(get("$.hasura.claims.x"), Some(json!(1)));
        assert_eq!(get("$.roles[1]"), Some(json!("b")));
        assert_eq!(get("$['a.b'].c"), Some(json!(2)));
        assert_eq!(get("$.['a.b'].c"), Some(json!(2)));
        assert_eq!(get("$.roles.[0]"), Some(json!("a")));
        assert_eq!(get("$[\"hasura\"]['claims']"), Some(json!({"x": 1})));
        assert_eq!(get("$"), Some(v.clone()));
        assert_eq!(get("hasura.claims.x"), Some(json!(1)));
        assert_eq!(get("$.missing"), None);
        assert_eq!(get("$.roles[5]"), None);
        assert!(JsonPath::parse("$.").is_err());
        assert!(JsonPath::parse("$[x]").is_err());
        assert!(JsonPath::parse("$['x'").is_err());
    }
}
