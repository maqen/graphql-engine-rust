//! JSON the way Go's `encoding/json` reads and writes it: `json.Indent` over raw text and
//! `json.Marshal` of decoded values (maps with sorted keys, numbers as `float64`).

use serde_json::Value;

use crate::gofmt::json_string;

/// `json.Indent(dst, src, "", indent)`: re-indent raw JSON text without touching its
/// tokens. `None` when the text is not valid JSON.
pub fn indent(src: &str, indent: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(src).ok()?;
    let mut out = String::with_capacity(src.len() * 2);
    let mut depth = 0usize;
    let mut need_indent = false;
    let mut chars = src.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent);
        }
    };
    while let Some(c) = chars.next() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if need_indent && c != ']' && c != '}' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            '"' => {
                out.push('"');
                let mut escaped = false;
                for c in chars.by_ref() {
                    out.push(c);
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        break;
                    }
                }
            }
            '{' | '[' => {
                out.push(c);
                need_indent = true;
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '}' | ']' => {
                if need_indent {
                    // An empty object or array stays on one line.
                    need_indent = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// `json.Marshal` (or `json.MarshalIndent` with `indent`) of a value Go decoded into `any`.
pub fn marshal(v: &Value, indent: Option<&str>) -> String {
    let mut out = String::new();
    write(&mut out, v, indent, 0);
    out
}

fn write(out: &mut String, v: &Value, indent: Option<&str>, depth: usize) {
    let newline = |out: &mut String, depth: usize| {
        if let Some(i) = indent {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(i);
            }
        }
    };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&float(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => out.push_str(&json_string(s)),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write(out, x, indent, depth + 1);
            }
            newline(out, depth);
            out.push(']');
        }
        Value::Object(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                out.push_str(&json_string(k));
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write(out, &o[k.as_str()], indent, depth + 1);
            }
            newline(out, depth);
            out.push('}');
        }
    }
}

/// encoding/json's float64 format: shortest representation, `%e` style outside
/// [1e-6, 1e21), with a two-digit-minimum exponent trimmed to its natural width.
pub fn float(f: f64) -> String {
    if f == 0.0 {
        return "0".into();
    }
    let abs = f.abs();
    if !(1e-6..1e21).contains(&abs) {
        // Rust's `{:e}` is the shortest round-trip form: "1.5e-7", "1e21".
        let s = format!("{f:e}");
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mant}e{sign}{:02}", exp.abs())
            .replace("e-0", "e-")
            .replacen("e+0", "e+", 1);
    }
    let s = format!("{f}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// `fmt.Sprintf("%v", x)` of a value decoded into `any`.
pub fn go_value(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            // %v of a float64 is %g: shortest, exponent form for exp < -4 || exp >= 21.
            let abs = f.abs();
            if f != 0.0 && !(1e-4..1e21).contains(&abs) {
                let s = format!("{f:e}");
                let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
                let exp: i32 = exp.parse().unwrap_or(0);
                let sign = if exp < 0 { '-' } else { '+' };
                format!("{mant}e{sign}{:02}", exp.abs())
            } else {
                let s = format!("{f}");
                s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("[{}]", a.iter().map(go_value).collect::<Vec<_>>().join(" ")),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{k}:{}", go_value(&o[k.as_str()])))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_like_go() {
        assert_eq!(
            indent(r#"{"a":[1,2],"b":{},"c":[],"d":"x\"y"}"#, "  ").unwrap(),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": [],\n  \"d\": \"x\\\"y\"\n}"
        );
        assert_eq!(indent("[]", "  ").unwrap(), "[]");
        assert!(indent("{", "  ").is_none());
    }

    #[test]
    fn marshals_like_go() {
        let v: Value =
            serde_json::from_str(r#"{"b":1.0,"a":"<x>","c":[1e21,0.0000001,12.5]}"#).unwrap();
        assert_eq!(
            marshal(&v, None),
            r#"{"a":"\u003cx\u003e","b":1,"c":[1e+21,1e-7,12.5]}"#
        );
    }
}
