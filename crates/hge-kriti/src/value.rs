//! aeson `Value` semantics on top of `serde_json::Value`: numeric equality, the derived
//! `Ord`, sorted-key objects and aeson's compact encoding.

use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::fmt::Write;

/// aeson's `typeOf` / kriti's `typeOfJSON`.
pub(crate) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "Object",
        Value::Array(_) => "Array",
        Value::String(_) => "String",
        Value::Number(_) => "Number",
        Value::Bool(_) => "Boolean",
        Value::Null => "Null",
    }
}

fn num_cmp(a: &Number, b: &Number) -> Ordering {
    let int = |n: &Number| {
        n.as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
    };
    match (int(a), int(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => {
            let (x, y) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
    }
}

/// Structural equality with numeric comparison (`1 == 1.0`) and unordered objects.
pub(crate) fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => num_cmp(x, y) == Ordering::Equal,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Object(_) => 0,
        Value::Array(_) => 1,
        Value::String(_) => 2,
        Value::Number(_) => 3,
        Value::Bool(_) => 4,
        Value::Null => 5,
    }
}

fn sorted_entries(m: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<_> = m.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// Haskell's derived `Ord` on aeson `Value`:
/// Object < Array < String < Number < Bool < Null across constructors.
pub(crate) fn json_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => num_cmp(x, y),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Array(x), Value::Array(y)) => {
            for (a, b) in x.iter().zip(y) {
                match json_cmp(a, b) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            x.len().cmp(&y.len())
        }
        (Value::Object(x), Value::Object(y)) => {
            let (x, y) = (sorted_entries(x), sorted_entries(y));
            for ((ka, va), (kb, vb)) in x.iter().zip(&y) {
                match ka.cmp(kb).then_with(|| json_cmp(va, vb)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            x.len().cmp(&y.len())
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

/// `Scientific.toBoundedInteger @Int`: the number as an `i64` if it is integral and in range.
pub(crate) fn as_int(n: &Number) -> Option<i64> {
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    if n.is_u64() {
        return None;
    }
    let f = n.as_f64()?;
    // `i64::MAX as f64` rounds up to 2^63, so the upper bound is exclusive.
    (f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64).then_some(f as i64)
}

/// Builds an object with keys inserted in sorted order (aeson's Map-based `KeyMap`).
pub(crate) fn sorted_object(mut entries: Vec<(String, Value)>) -> Value {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(entries.into_iter().collect())
}

/// Recursively re-orders object keys, rebuilding only objects that are out of order.
pub(crate) fn sort_keys(v: &mut Value) {
    match v {
        Value::Array(items) => items.iter_mut().for_each(sort_keys),
        Value::Object(map) => {
            map.values_mut().for_each(sort_keys);
            let ordered = map.keys().zip(map.keys().skip(1)).all(|(a, b)| a < b);
            if !ordered {
                let entries = std::mem::take(map).into_iter().collect();
                *v = sorted_object(entries);
            }
        }
        _ => {}
    }
}

/// aeson's compact `encode` with sorted keys.
pub(crate) fn encode(v: &Value) -> String {
    let mut out = String::new();
    encode_into(v, &mut out);
    out
}

fn encode_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => encode_number(n, out),
        Value::String(s) => encode_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_into(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in sorted_entries(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(k, out);
                out.push(':');
                encode_into(v, out);
            }
            out.push('}');
        }
    }
}

fn encode_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Integers print exactly; floats use `Scientific`'s `Generic` format (`1.0`, `0.25`,
/// `1.0e-2`, `1.2345678e8`), i.e. exponent notation outside `0.1 <= |x| < 10^7`.
fn encode_number(n: &Number, out: &mut String) {
    if n.is_i64() || n.is_u64() {
        let _ = write!(out, "{n}");
        return;
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f == 0.0 {
        out.push_str("0.0");
        return;
    }
    if f < 0.0 {
        out.push('-');
    }
    // Shortest round-trip digits: `d.ddde±x` means 0.dddd × 10^(x+1).
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let e = exp.parse::<i64>().unwrap_or(0) + 1;
    if !(0..=7).contains(&e) {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() { "0" } else { rest };
        let _ = write!(out, "{first}.{rest}e{}", e - 1);
    } else if e == 0 {
        let _ = write!(out, "0.{digits}");
    } else {
        let e = e as usize;
        if digits.len() <= e {
            let _ = write!(out, "{digits}{}.0", "0".repeat(e - digits.len()));
        } else {
            let _ = write!(out, "{}.{}", &digits[..e], &digits[e..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_equality_and_ordering() {
        assert!(json_eq(&json!(1), &json!(1.0)));
        assert!(json_eq(
            &json!({"a": [1], "b": 2}),
            &json!({"b": 2.0, "a": [1.0]})
        ));
        assert_eq!(json_cmp(&json!(2), &json!(10.5)), Ordering::Less);
        assert_eq!(json_cmp(&json!("b"), &json!("a")), Ordering::Greater);
        assert_eq!(json_cmp(&json!({}), &json!([])), Ordering::Less);
        assert_eq!(json_cmp(&json!(1), &json!(true)), Ordering::Less);
        assert_eq!(json_cmp(&json!(false), &json!(true)), Ordering::Less);
        assert_eq!(json_cmp(&json!(true), &json!(null)), Ordering::Less);
    }

    #[test]
    fn aeson_encoding() {
        let v: Value = serde_json::from_str(r#"{"b":[1,2.5,null],"a":"x\"\n\u0001"}"#).unwrap();
        assert_eq!(encode(&v), r#"{"a":"x\"\n\u0001","b":[1,2.5,null]}"#);
        let cases = [
            (1.0, "1.0"),
            (0.25, "0.25"),
            (0.01, "1.0e-2"),
            (123456789.0, "1.23456789e8"),
            (1234567.5, "1234567.5"),
            (-2.0, "-2.0"),
            (100.0, "100.0"),
        ];
        for (f, s) in cases {
            assert_eq!(encode(&json!(f)), s, "{f}");
        }
    }

    #[test]
    fn sorts_keys_deeply() {
        let mut v: Value = serde_json::from_str(r#"[{"b":1,"a":{"d":1,"c":2}}]"#).unwrap();
        sort_keys(&mut v);
        assert_eq!(v.to_string(), r#"[{"a":{"c":2,"d":1},"b":1}]"#);
    }
}
