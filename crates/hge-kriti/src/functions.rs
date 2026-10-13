//! Kriti's `basicFuncMap`, user-supplied custom functions and Hasura's `getSessionVariable`.

use crate::value::{sorted_object, type_name};
use serde_json::{Map, Number, Value};
use std::collections::HashMap;
use std::fmt;

/// A custom function: one JSON argument in, a JSON value or an error message out.
pub type CustomFn = Box<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

/// The functions a template may call: kriti's basic functions plus custom ones.
///
/// Basic functions always win over a custom function with the same name (Hasura unions
/// the maps left-biased with the basic map first).
#[derive(Default)]
pub struct Functions {
    custom: HashMap<String, CustomFn>,
}

impl fmt::Debug for Functions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.custom.keys().collect();
        names.sort();
        f.debug_struct("Functions").field("custom", &names).finish()
    }
}

impl Functions {
    /// Only the basic functions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a custom function (ignored at call time if it shadows a basic one).
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        f: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> &mut Self {
        self.custom.insert(name.into(), Box::new(f));
        self
    }

    /// Builder form of [`Functions::insert`].
    pub fn with(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.insert(name, f);
        self
    }

    /// Calls `name`; `None` if no such function exists.
    pub fn call(&self, name: &str, arg: &Value) -> Option<Result<Value, String>> {
        basic(name)
            .map(|f| f(arg))
            .or_else(|| self.custom.get(name).map(|f| f(arg)))
    }
}

type BasicFn = fn(&Value) -> Result<Value, String>;

fn basic(name: &str) -> Option<BasicFn> {
    Some(match name {
        "empty" => empty,
        "size" => size,
        "inverse" => inverse,
        "head" => head,
        "tail" => tail,
        "toCaseFold" => to_case_fold,
        "toLower" => to_lower,
        "toUpper" => to_upper,
        "toTitle" => to_title,
        "escapeUri" => escape_uri,
        "fromPairs" => from_pairs,
        "toPairs" => to_pairs,
        "removeNulls" => remove_nulls,
        "concat" => concat,
        "not" => not,
        _ => return None,
    })
}

/// aeson's `withX name` failure message.
fn mismatch(name: &str, expected: &str, got: &Value) -> String {
    format!(
        "parsing {name} failed, expected {expected}, but encountered {}",
        type_name(got)
    )
}

fn with_text(v: &Value) -> Result<&str, String> {
    v.as_str().ok_or_else(|| mismatch("String", "String", v))
}

/// Haskell's `Data.Char.isSpace`.
fn is_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

fn empty(v: &Value) -> Result<Value, String> {
    Ok(Value::Bool(match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.trim_matches(is_space).is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Bool(_) => return Err("Cannot define emptiness for a boolean".into()),
        Value::Null => true,
    }))
}

fn size(v: &Value) -> Result<Value, String> {
    Ok(match v {
        Value::Object(m) => m.len().into(),
        Value::Array(a) => a.len().into(),
        Value::String(s) => s.chars().count().into(),
        Value::Number(_) => v.clone(),
        Value::Bool(b) => u8::from(*b).into(),
        Value::Null => 0.into(),
    })
}

fn inverse(v: &Value) -> Result<Value, String> {
    Ok(match v {
        Value::Array(a) => Value::Array(a.iter().rev().cloned().collect()),
        Value::String(s) => Value::String(s.chars().rev().collect()),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f == 0.0 {
                return Err("Division by zero".into());
            }
            let r = 1.0 / f;
            if r.fract() == 0.0 && r.abs() < 9.007_199_254_740_992e15 {
                (r as i64).into()
            } else {
                Number::from_f64(r).map_or(Value::Null, Value::Number)
            }
        }
        Value::Bool(b) => Value::Bool(!b),
        Value::Object(_) | Value::Null => v.clone(),
    })
}

fn head(v: &Value) -> Result<Value, String> {
    match v {
        Value::Array(a) => a.first().cloned().ok_or_else(|| "Empty array".into()),
        Value::String(s) => s
            .chars()
            .next()
            .map(|c| Value::String(c.to_string()))
            .ok_or_else(|| "Empty string".into()),
        _ => Err("Expected an array or string".into()),
    }
}

/// Haskell crashes on an empty input; we report a function error instead.
fn tail(v: &Value) -> Result<Value, String> {
    match v {
        Value::Array(a) if a.is_empty() => Err("Empty array".into()),
        Value::Array(a) => Ok(Value::Array(a[1..].to_vec())),
        Value::String(s) => {
            let mut chars = s.chars();
            chars.next().ok_or("Empty string")?;
            Ok(Value::String(chars.as_str().into()))
        }
        _ => Err("Expected an array or string".into()),
    }
}

/// Unicode case folding, approximated by per-character lowercasing plus the common
/// full-folding special cases (Rust's std has no case-fold table).
fn to_case_fold(v: &Value) -> Result<Value, String> {
    let s = with_text(v)?;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ſ' => out.push('s'),
            'ς' => out.push('σ'),
            'µ' => out.push('μ'),
            'ϐ' => out.push('β'),
            'ϑ' => out.push('θ'),
            'ϕ' => out.push('φ'),
            'ϖ' => out.push('π'),
            'ϰ' => out.push('κ'),
            'ϱ' => out.push('ρ'),
            'ϵ' => out.push('ε'),
            'ŉ' => out.push_str("ʼn"),
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            c => out.extend(c.to_lowercase()),
        }
    }
    Ok(Value::String(out))
}

/// `Data.Text.toLower`: per-character mapping (no final-sigma context rule).
fn to_lower(v: &Value) -> Result<Value, String> {
    let s = with_text(v)?;
    Ok(Value::String(
        s.chars().flat_map(char::to_lowercase).collect(),
    ))
}

fn to_upper(v: &Value) -> Result<Value, String> {
    Ok(Value::String(with_text(v)?.to_uppercase()))
}

fn push_title(c: char, out: &mut String) {
    match c {
        'Ǆ' | 'ǅ' | 'ǆ' => out.push('ǅ'),
        'Ǉ' | 'ǈ' | 'ǉ' => out.push('ǈ'),
        'Ǌ' | 'ǋ' | 'ǌ' => out.push('ǋ'),
        'Ǳ' | 'ǲ' | 'ǳ' => out.push('ǲ'),
        'ß' => out.push_str("Ss"),
        'ﬀ' => out.push_str("Ff"),
        'ﬁ' => out.push_str("Fi"),
        'ﬂ' => out.push_str("Fl"),
        'ﬃ' => out.push_str("Ffi"),
        'ﬄ' => out.push_str("Ffl"),
        'ﬅ' | 'ﬆ' => out.push_str("St"),
        c => out.extend(c.to_uppercase()),
    }
}

/// `Data.Text.toTitle`: the first letter of each word is title-cased, the rest lowercased.
/// A word starts at a non-space character that follows a non-letter.
fn to_title(v: &Value) -> Result<Value, String> {
    let s = with_text(v)?;
    let mut out = String::with_capacity(s.len());
    let mut in_word = false;
    for c in s.chars() {
        if is_space(c) {
            out.push(c);
            in_word = false;
        } else if in_word {
            out.extend(c.to_lowercase());
        } else {
            push_title(c, &mut out);
            in_word = c.is_alphabetic();
        }
    }
    Ok(Value::String(out))
}

/// Percent-encodes everything but RFC 3986 unreserved characters (UTF-8, upper-case hex).
fn escape_uri(v: &Value) -> Result<Value, String> {
    let s = with_text(v)?;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(Value::String(out))
}

fn from_pairs(v: &Value) -> Result<Value, String> {
    let items = v
        .as_array()
        .ok_or_else(|| mismatch("Nested Arrays", "Array", v))?;
    let mut map: Map<String, Value> = Map::new();
    for item in items {
        let pair = item
            .as_array()
            .ok_or_else(|| mismatch("Array of Pair", "Array", item))?;
        let [k, val] = pair.as_slice() else {
            return Err(
                "Expected an array of shape [ [k1,v1], [k2,v2] ... ] - With String keys.".into(),
            );
        };
        let k = k.as_str().ok_or_else(|| mismatch("Key", "String", k))?;
        map.insert(k.to_owned(), val.clone());
    }
    Ok(sorted_object(map.into_iter().collect()))
}

fn to_pairs(v: &Value) -> Result<Value, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| mismatch("Object", "Object", v))?;
    let mut pairs: Vec<_> = obj.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    Ok(Value::Array(
        pairs
            .into_iter()
            .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), v.clone()]))
            .collect(),
    ))
}

fn remove_nulls(v: &Value) -> Result<Value, String> {
    let items = v.as_array().ok_or_else(|| mismatch("Array", "Array", v))?;
    Ok(Value::Array(
        items.iter().filter(|v| !v.is_null()).cloned().collect(),
    ))
}

/// Flattens arrays, joins strings or merges objects (later keys win), tried in that order.
fn concat(v: &Value) -> Result<Value, String> {
    let items = v.as_array().ok_or_else(|| mismatch("Array", "Array", v))?;
    if items.iter().all(Value::is_array) {
        return Ok(Value::Array(
            items
                .iter()
                .filter_map(Value::as_array)
                .flatten()
                .cloned()
                .collect(),
        ));
    }
    if items.iter().all(Value::is_string) {
        return Ok(Value::String(
            items.iter().filter_map(Value::as_str).collect(),
        ));
    }
    if let Some(bad) = items.iter().find(|v| !v.is_object()) {
        return Err(mismatch("Nested Object", "Object", bad));
    }
    let mut merged: Map<String, Value> = Map::new();
    for obj in items.iter().filter_map(Value::as_object) {
        for (k, v) in obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    Ok(sorted_object(merged.into_iter().collect()))
}

fn not(v: &Value) -> Result<Value, String> {
    v.as_bool()
        .map(|b| Value::Bool(!b))
        .ok_or_else(|| mismatch("Bool", "Boolean", v))
}

/// Hasura's `getSessionVariable(name)`: case-insensitive lookup in the session variables.
///
/// A `null` argument yields `null`; a missing variable (or no session) is the error
/// `Session variable "<name>" not found`; a non-string name is an error too.
pub fn get_session_variable_fn(session: Option<Map<String, Value>>) -> CustomFn {
    let session: Option<HashMap<String, Value>> =
        session.map(|m| m.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect());
    Box::new(move |arg| match arg {
        Value::Null => Ok(Value::Null),
        Value::String(name) => session
            .as_ref()
            .and_then(|s| s.get(&name.to_lowercase()))
            .cloned()
            .ok_or_else(|| format!("Session variable \"{name}\" not found")),
        _ => Err("Session variable name should be a string".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arg: Value) -> Result<Value, String> {
        Functions::new().call(name, &arg).expect("known function")
    }

    #[test]
    fn empty_fn() {
        assert_eq!(call("empty", json!({})), Ok(json!(true)));
        assert_eq!(call("empty", json!([1])), Ok(json!(false)));
        assert_eq!(call("empty", json!(" \t\n")), Ok(json!(true)));
        assert_eq!(call("empty", json!(" a ")), Ok(json!(false)));
        assert_eq!(call("empty", json!(0)), Ok(json!(true)));
        assert_eq!(call("empty", json!(0.0)), Ok(json!(true)));
        assert_eq!(call("empty", json!(null)), Ok(json!(true)));
        assert_eq!(
            call("empty", json!(true)),
            Err("Cannot define emptiness for a boolean".into())
        );
    }

    #[test]
    fn size_fn() {
        assert_eq!(call("size", json!({"a": 1, "b": 2})), Ok(json!(2)));
        assert_eq!(call("size", json!([1, 2, 3])), Ok(json!(3)));
        assert_eq!(call("size", json!("σσ")), Ok(json!(2)));
        assert_eq!(call("size", json!(4.5)), Ok(json!(4.5)));
        assert_eq!(call("size", json!(true)), Ok(json!(1)));
        assert_eq!(call("size", json!(false)), Ok(json!(0)));
        assert_eq!(call("size", json!(null)), Ok(json!(0)));
    }

    #[test]
    fn inverse_fn() {
        assert_eq!(call("inverse", json!([1, 2])), Ok(json!([2, 1])));
        assert_eq!(call("inverse", json!("abc")), Ok(json!("cba")));
        assert_eq!(call("inverse", json!(4)), Ok(json!(0.25)));
        assert_eq!(call("inverse", json!(0.5)), Ok(json!(2)));
        assert_eq!(call("inverse", json!(true)), Ok(json!(false)));
        assert_eq!(call("inverse", json!({"a": 1})), Ok(json!({"a": 1})));
        assert_eq!(call("inverse", json!(null)), Ok(json!(null)));
        assert!(call("inverse", json!(0)).is_err());
    }

    #[test]
    fn head_and_tail() {
        assert_eq!(call("head", json!([1, 2])), Ok(json!(1)));
        assert_eq!(call("head", json!("σx")), Ok(json!("σ")));
        assert_eq!(call("head", json!([])), Err("Empty array".into()));
        assert_eq!(call("head", json!("")), Err("Empty string".into()));
        assert_eq!(
            call("head", json!(1)),
            Err("Expected an array or string".into())
        );
        assert_eq!(call("tail", json!([1, 2])), Ok(json!([2])));
        assert_eq!(call("tail", json!("σxy")), Ok(json!("xy")));
        assert!(call("tail", json!([])).is_err());
        assert_eq!(
            call("tail", json!({})),
            Err("Expected an array or string".into())
        );
    }

    #[test]
    fn case_functions() {
        assert_eq!(call("toLower", json!("HeLLo ΣΑΣ")), Ok(json!("hello σασ")));
        assert_eq!(call("toUpper", json!("straße")), Ok(json!("STRASSE")));
        assert_eq!(call("toCaseFold", json!("Straße")), Ok(json!("strasse")));
        assert_eq!(
            call("toTitle", json!("hello wORLD o'neil 1abc")),
            Ok(json!("Hello World O'neil 1Abc"))
        );
        assert_eq!(
            call("toLower", json!(1)),
            Err("parsing String failed, expected String, but encountered Number".into())
        );
        assert_eq!(
            call("toTitle", json!(null)),
            Err("parsing String failed, expected String, but encountered Null".into())
        );
    }

    #[test]
    fn escape_uri_fn() {
        assert_eq!(
            call("escapeUri", json!("?foo=bar/baz")),
            Ok(json!("%3Ffoo%3Dbar%2Fbaz"))
        );
        assert_eq!(
            call("escapeUri", json!("a-._~ σ")),
            Ok(json!("a-._~%20%CF%83"))
        );
        assert!(call("escapeUri", json!([])).is_err());
    }

    #[test]
    fn pairs() {
        assert_eq!(
            call("fromPairs", json!([["b", 1], ["a", 2], ["b", 3]])).map(|v| v.to_string()),
            Ok(r#"{"a":2,"b":3}"#.into())
        );
        assert_eq!(
            call("fromPairs", json!([["a"]])),
            Err("Expected an array of shape [ [k1,v1], [k2,v2] ... ] - With String keys.".into())
        );
        assert_eq!(
            call("fromPairs", json!([[1, 2]])),
            Err("parsing Key failed, expected String, but encountered Number".into())
        );
        assert_eq!(
            call("fromPairs", json!([1])),
            Err("parsing Array of Pair failed, expected Array, but encountered Number".into())
        );
        assert_eq!(
            call("fromPairs", json!({})),
            Err("parsing Nested Arrays failed, expected Array, but encountered Object".into())
        );
        let unsorted: Value = serde_json::from_str(r#"{"c":"d","a":"b"}"#).unwrap();
        assert_eq!(
            call("toPairs", unsorted),
            Ok(json!([["a", "b"], ["c", "d"]]))
        );
        assert_eq!(
            call("toPairs", json!([])),
            Err("parsing Object failed, expected Object, but encountered Array".into())
        );
    }

    #[test]
    fn remove_nulls_and_concat() {
        assert_eq!(call("removeNulls", json!([1, null, 2])), Ok(json!([1, 2])));
        assert_eq!(
            call("removeNulls", json!("x")),
            Err("parsing Array failed, expected Array, but encountered String".into())
        );
        assert_eq!(call("concat", json!([])), Ok(json!([])));
        assert_eq!(call("concat", json!([[1], [2, 3]])), Ok(json!([1, 2, 3])));
        assert_eq!(call("concat", json!(["a", "b"])), Ok(json!("ab")));
        assert_eq!(
            call("concat", json!([{"a": 1, "b": 2}, {"b": 3, "c": 4}])),
            Ok(json!({"a": 1, "b": 3, "c": 4}))
        );
        assert_eq!(
            call("concat", json!([{"a": 1}, [1]])),
            Err("parsing Nested Object failed, expected Object, but encountered Array".into())
        );
        assert_eq!(
            call("concat", json!(1)),
            Err("parsing Array failed, expected Array, but encountered Number".into())
        );
    }

    #[test]
    fn not_fn() {
        assert_eq!(call("not", json!(true)), Ok(json!(false)));
        assert_eq!(
            call("not", json!("x")),
            Err("parsing Bool failed, expected Boolean, but encountered String".into())
        );
    }

    #[test]
    fn custom_functions_cannot_override_basic_ones() {
        let fns = Functions::new()
            .with("size", |_| Ok(json!("custom")))
            .with("double", |v| Ok(json!(v.as_i64().unwrap_or(0) * 2)));
        assert_eq!(fns.call("size", &json!([1])), Some(Ok(json!(1))));
        assert_eq!(fns.call("double", &json!(21)), Some(Ok(json!(42))));
        assert_eq!(fns.call("missing", &json!(1)), None);
    }

    #[test]
    fn session_variable_lookup() {
        let session = json!({"x-hasura-user-id": "42", "X-Hasura-Role": "user"});
        let f = get_session_variable_fn(session.as_object().cloned());
        assert_eq!(f(&json!("X-HASURA-USER-ID")), Ok(json!("42")));
        assert_eq!(f(&json!("x-hasura-role")), Ok(json!("user")));
        assert_eq!(f(&json!(null)), Ok(json!(null)));
        assert_eq!(
            f(&json!("x-hasura-org")),
            Err("Session variable \"x-hasura-org\" not found".into())
        );
        assert_eq!(
            f(&json!(1)),
            Err("Session variable name should be a string".into())
        );
        let none = get_session_variable_fn(None);
        assert_eq!(
            none(&json!("a")),
            Err("Session variable \"a\" not found".into())
        );
    }
}
