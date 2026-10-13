//! JSON encoding that matches V2's (aeson 2.x) byte for byte.
//!
//! V2 builds webhook bodies and invocation logs through an aeson `Value`, whose objects are
//! ordered maps, so every object comes out with its keys sorted. Numbers go through
//! `Scientific` (so `1.50` becomes `1.5` and `0.05` becomes `5.0e-2`) and strings use aeson's
//! escaping. [`Json`] keeps number literals as text, so nothing is lost to `f64`.

use std::collections::BTreeMap;
use std::fmt::Write;

/// A JSON value with sorted object keys and verbatim number literals.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// A number literal as it appeared in the input (re-rendered by aeson rules on output).
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::String(s.into())
    }

    pub fn int(n: i64) -> Json {
        Json::Number(n.to_string())
    }

    pub fn object<K: Into<String>>(entries: impl IntoIterator<Item = (K, Json)>) -> Json {
        Json::Object(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Parse JSON text (as produced by Postgres or any other source).
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        p.ws();
        let v = p.value(0)?;
        p.ws();
        if p.i != p.s.len() {
            return Err(format!("trailing characters at offset {}", p.i));
        }
        Ok(v)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// Encode exactly like aeson's `encode`.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(&aeson_number(n)),
            Json::String(s) => write_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Object(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl From<&serde_json::Value> for Json {
    fn from(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Json::Null,
            serde_json::Value::Bool(b) => Json::Bool(*b),
            serde_json::Value::Number(n) => Json::Number(n.to_string()),
            serde_json::Value::String(s) => Json::String(s.clone()),
            serde_json::Value::Array(a) => Json::Array(a.iter().map(Json::from).collect()),
            serde_json::Value::Object(m) => {
                Json::Object(m.iter().map(|(k, v)| (k.clone(), Json::from(v))).collect())
            }
        }
    }
}

/// aeson's string escaping: `"`, `\`, `\n`, `\r`, `\t`, other control characters as
/// `\u00xx` (lowercase hex). Everything else, including non-ASCII, is written as is.
fn write_string(s: &str, out: &mut String) {
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

/// Render a JSON number literal the way aeson renders the `Scientific` it parses to.
///
/// aeson writes a number whose parsed exponent is `>= 0` as an integer; otherwise it uses
/// `formatScientific Generic`: fixed notation when the normalised decimal exponent is in
/// `0..=7`, else `d.ddde<n>`.
fn aeson_number(lit: &str) -> String {
    let Some((negative, digits, exp)) = parse_scientific(lit) else {
        return lit.to_owned();
    };
    let sign = if negative { "-" } else { "" };
    // Strip leading zeros of the coefficient.
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        // zero: an integer literal stays `0`, a fractional one becomes `0.0`.
        return if exp >= 0 {
            "0".to_owned()
        } else {
            "0.0".to_owned()
        };
    }
    if exp >= 0 {
        let mut s = String::with_capacity(digits.len() + exp as usize + 1);
        s.push_str(sign);
        s.push_str(digits);
        for _ in 0..exp {
            s.push('0');
        }
        return s;
    }
    // Normalise: value = 0.d1d2... * 10^e (toDecimalDigits), trailing zeros removed.
    let ds = digits.trim_end_matches('0');
    let e = digits.len() as i64 + exp;
    let mut s = String::from(sign);
    if !(0..=7).contains(&e) {
        // FFExponent
        let (first, rest) = ds.split_at(1);
        s.push_str(first);
        s.push('.');
        s.push_str(if rest.is_empty() { "0" } else { rest });
        let _ = write!(s, "e{}", e - 1);
    } else if e == 0 {
        s.push_str("0.");
        s.push_str(ds);
    } else {
        let e = e as usize;
        if ds.len() <= e {
            s.push_str(ds);
            for _ in ds.len()..e {
                s.push('0');
            }
            s.push_str(".0");
        } else {
            s.push_str(&ds[..e]);
            s.push('.');
            s.push_str(&ds[e..]);
        }
    }
    s
}

/// Split a JSON number literal into (negative, coefficient digits, base-10 exponent).
fn parse_scientific(lit: &str) -> Option<(bool, String, i64)> {
    let (negative, rest) = match lit.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, lit),
    };
    let (mantissa, exp) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], rest[i + 1..].parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((negative, format!("{int}{frac}"), exp - frac.len() as i64))
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

const MAX_DEPTH: usize = 512;

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("{msg} at offset {}", self.i))
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        match self.s.get(self.i) {
            None => self.err("unexpected end of input"),
            Some(b'n') => self.lit("null", Json::Null),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'[') => {
                self.i += 1;
                let mut items = vec![];
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut map = BTreeMap::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Object(map));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return self.err("expected object key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return self.err("expected ':'");
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    map.insert(k, v);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Object(map));
                        }
                        _ => return self.err("expected ',' or '}'"),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.i;
                if self.s[self.i] == b'-' {
                    self.i += 1;
                }
                while self.i < self.s.len()
                    && matches!(
                        self.s[self.i],
                        b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                    )
                {
                    self.i += 1;
                }
                let lit = std::str::from_utf8(&self.s[start..self.i]).expect("ascii");
                if parse_scientific(lit).is_none() {
                    return self.err("invalid number");
                }
                Ok(Json::Number(lit.to_owned()))
            }
            Some(_) => self.err("unexpected character"),
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("invalid literal")
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match h {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => self.err("invalid \\u escape"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'"' && self.s[self.i] != b'\\' {
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?);
            match self.s.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(_) => {
                    self.i += 1;
                    let c = match self.s.get(self.i) {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            self.i += 1;
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi)
                                && self.s.get(self.i..self.i + 2) == Some(b"\\u")
                            {
                                self.i += 2;
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            continue;
                        }
                        _ => return self.err("invalid escape"),
                    };
                    self.i += 1;
                    out.push(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(s: &str) -> String {
        Json::parse(s).unwrap().encode()
    }

    #[test]
    fn sorts_keys_at_every_level() {
        assert_eq!(
            enc(r#"{"b": 1, "a": {"z": [ {"y":1,"x":2} ], "c": null}}"#),
            r#"{"a":{"c":null,"z":[{"x":2,"y":1}]},"b":1}"#
        );
    }

    #[test]
    fn numbers_like_aeson() {
        for (input, want) in [
            ("1", "1"),
            ("-17", "-17"),
            ("0", "0"),
            ("1.0", "1.0"),
            ("1.50", "1.5"),
            ("0.5", "0.5"),
            ("0.05", "5.0e-2"),
            ("123.456", "123.456"),
            ("1e3", "1000"),
            ("1.5e3", "1500"),
            ("12345678.5", "1.23456785e7"),
            ("1234567.5", "1234567.5"),
            ("100.00", "100.0"),
            ("0.0", "0.0"),
            ("-2.50", "-2.5"),
            (
                "123456789012345678901234567890",
                "123456789012345678901234567890",
            ),
        ] {
            assert_eq!(aeson_number(input), want, "{input}");
        }
    }

    #[test]
    fn strings_like_aeson() {
        assert_eq!(
            enc(r#""a\"b\\c\/d\b\f\n\r\t\u0001éé😀😀""#),
            "\"a\\\"b\\\\c/d\\u0008\\u000c\\n\\r\\t\\u0001éé😀😀\""
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(Json::parse("{").is_err());
        assert!(Json::parse("[1,]").is_err());
        assert!(Json::parse("1 2").is_err());
        assert!(Json::parse("1.2.3").is_err());
    }
}
