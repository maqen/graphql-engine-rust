//! Decoding node ids and cursors with V2's (aeson's) error messages.

use hge_core::ColumnType;
use serde_json::{Map, Number, Value as Json};

/// A decoded node id: `[1, "<schema>", "<table>", <key values>…]`.
#[derive(Debug, PartialEq)]
pub(crate) struct NodeId {
    pub schema: String,
    pub table: String,
    pub values: Vec<Scalar>,
}

/// A JSON value with numbers kept exactly (aeson's `Scientific`): node ids may hold numbers
/// outside `f64`, and errors print them as Haskell does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Scalar {
    Null,
    Bool(bool),
    Number(Sci),
    String(String),
    Array(Vec<Scalar>),
    Object(Vec<(String, Scalar)>),
}

/// `±0.d₁d₂…dₙ × 10^exponent` (digits without leading or trailing zeros; empty for 0).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sci {
    negative: bool,
    digits: Vec<u8>,
    exponent: i64,
}

/// Base64 decoding that skips characters outside the alphabet and tolerates missing
/// padding (`Data.ByteString.Base64.decodeLenient`).
pub(crate) fn base64_decode_lenient(s: &str) -> Vec<u8> {
    let mut bits: u32 = 0;
    let mut n = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        bits = (bits << 6) | u32::from(v);
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    out
}

/// Parse a node id (after base64 decoding), with aeson's messages.
pub(crate) fn parse_node_id(bytes: &[u8]) -> Result<NodeId, String> {
    let value = parse_json(bytes)?;
    let in_root = |m: &str| format!("Error in $: {m}");
    let Scalar::Array(items) = value else {
        return Err(in_root(&format!(
            "parsing node id failed, expected Array, but encountered {}",
            kind(&value)
        )));
    };
    let mut items = items.into_iter();
    match items.next() {
        None => Err(in_root("unexpected GUID format, found empty list")),
        Some(Scalar::Number(v)) if v.is_one() => {
            let rest: Vec<Scalar> = items.collect();
            if rest.len() < 3 {
                return Err(in_root(
                    "GUID version 1: expecting schema name, table name and at least one column value",
                ));
            }
            let mut rest = rest.into_iter();
            let text = |v: Scalar| match v {
                Scalar::String(s) => Ok(s),
                other => Err(in_root(&format!(
                    "parsing Text failed, expected String, but encountered {}",
                    kind(&other)
                ))),
            };
            let schema = text(rest.next().expect("checked length"))?;
            let table = text(rest.next().expect("checked length"))?;
            Ok(NodeId {
                schema,
                table,
                values: rest.collect(),
            })
        }
        Some(Scalar::Number(v)) => Err(in_root(&format!(
            "unsupported GUID version: {}",
            v.show()
        ))),
        Some(_) => Err(in_root(
            "unexpected GUID format, needs to start with a version number",
        )),
    }
}

/// The text of a value bound for a column of type `ty` (`None` for null), as V2's
/// `parseScalarValue` accepts it: strings are passed to Postgres as they are, other values
/// must match the type.
pub(crate) fn column_value(v: &Scalar, ty: &ColumnType) -> Result<Option<String>, String> {
    let int_kind = match (ty.dimensions, ty.name.as_str()) {
        (0, "int2" | "smallint") => Some(("PGSmallInt", i64::from(i16::MIN), i64::from(i16::MAX))),
        (0, "int4" | "integer" | "serial") => {
            Some(("PGInteger", i64::from(i32::MIN), i64::from(i32::MAX)))
        }
        (0, "int8" | "bigint" | "bigserial") => Some(("PGBigInt", i64::MIN, i64::MAX)),
        _ => None,
    };
    let texty = ty.dimensions == 0
        && matches!(
            ty.name.as_str(),
            "text" | "varchar" | "character varying" | "citext" | "bpchar" | "character"
        );
    match v {
        Scalar::Null => Ok(None),
        Scalar::String(s) => Ok(Some(s.clone())),
        Scalar::Number(n) if int_kind.is_some() => {
            let (_, min, max) = int_kind.expect("checked");
            match n.to_i64().filter(|i| (min..=max).contains(i)) {
                Some(i) => Ok(Some(i.to_string())),
                None => Err(format!(
                    "The value {} lies outside the bounds or is not an integer. Maybe it is a float, or is there integer overflow?",
                    n.show()
                )),
            }
        }
        other if int_kind.is_some() => Err(format!(
            "parsing Integer expected for input type: {} failed, expected Number, but encountered {}",
            int_kind.expect("checked").0,
            kind(other)
        )),
        other if texty => Err(format!(
            "parsing Text failed, expected String, but encountered {}",
            kind(other)
        )),
        Scalar::Bool(b) if ty.dimensions == 0 && matches!(ty.name.as_str(), "bool" | "boolean") => {
            Ok(Some(b.to_string()))
        }
        other if ty.dimensions == 0 && matches!(ty.name.as_str(), "bool" | "boolean") => {
            Err(format!(
                "parsing Bool failed, expected Boolean, but encountered {}",
                kind(other)
            ))
        }
        other => Ok(Some(other.to_json().to_string())),
    }
}

fn kind(v: &Scalar) -> &'static str {
    match v {
        Scalar::Null => "Null",
        Scalar::Bool(_) => "Boolean",
        Scalar::Number(_) => "Number",
        Scalar::String(_) => "String",
        Scalar::Array(_) => "Array",
        Scalar::Object(_) => "Object",
    }
}

impl Scalar {
    /// As JSON (numbers that do not fit `serde_json` become strings).
    pub(crate) fn to_json(&self) -> Json {
        match self {
            Scalar::Null => Json::Null,
            Scalar::Bool(b) => Json::Bool(*b),
            Scalar::Number(n) => n
                .to_json_number()
                .map(Json::Number)
                .unwrap_or_else(|| Json::String(n.show())),
            Scalar::String(s) => Json::String(s.clone()),
            Scalar::Array(items) => Json::Array(items.iter().map(Scalar::to_json).collect()),
            Scalar::Object(pairs) => {
                let mut m = Map::new();
                for (k, v) in pairs {
                    m.insert(k.clone(), v.to_json());
                }
                Json::Object(m)
            }
        }
    }

    /// From JSON (cursors, which are parsed with `serde_json`).
    pub(crate) fn from_json(v: &Json) -> Scalar {
        match v {
            Json::Null => Scalar::Null,
            Json::Bool(b) => Scalar::Bool(*b),
            Json::Number(n) => Sci::parse(&n.to_string())
                .map(Scalar::Number)
                .unwrap_or(Scalar::Null),
            Json::String(s) => Scalar::String(s.clone()),
            Json::Array(items) => Scalar::Array(items.iter().map(Scalar::from_json).collect()),
            Json::Object(m) => Scalar::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), Scalar::from_json(v)))
                    .collect(),
            ),
        }
    }
}

impl Sci {
    /// Parse a JSON number literal (already validated).
    fn parse(lit: &str) -> Option<Sci> {
        let (negative, rest) = match lit.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, lit),
        };
        let (mantissa, exp) = match rest.find(['e', 'E']) {
            Some(i) => (&rest[..i], rest[i + 1..].parse::<i64>().ok()?),
            None => (rest, 0),
        };
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let mut digits: Vec<u8> = int.bytes().chain(frac.bytes()).map(|b| b - b'0').collect();
        let mut exponent = exp + int.len() as i64;
        while digits.first() == Some(&0) {
            digits.remove(0);
            exponent -= 1;
        }
        while digits.last() == Some(&0) {
            digits.pop();
        }
        if digits.is_empty() {
            exponent = 0;
        }
        Some(Sci {
            negative,
            digits,
            exponent,
        })
    }

    fn is_one(&self) -> bool {
        !self.negative && self.digits == [1] && self.exponent == 1
    }

    fn to_i64(&self) -> Option<i64> {
        if self.digits.is_empty() {
            return Some(0);
        }
        if self.exponent < self.digits.len() as i64 || self.exponent > 19 {
            return None;
        }
        let mut v: i128 = 0;
        for i in 0..self.exponent {
            v = v * 10 + i128::from(*self.digits.get(i as usize).unwrap_or(&0));
        }
        if self.negative {
            v = -v;
        }
        i64::try_from(v).ok()
    }

    fn to_json_number(&self) -> Option<Number> {
        if let Some(i) = self.to_i64() {
            return Some(Number::from(i));
        }
        let f: f64 = self.show().parse().ok()?;
        f.is_finite().then(|| Number::from_f64(f)).flatten()
    }

    /// Haskell's `show` of a `Scientific`: `2.0`, `1.5`, `9.9999999999e10`, `5.0e-2`.
    fn show(&self) -> String {
        let digits: Vec<char> = if self.digits.is_empty() {
            vec!['0']
        } else {
            self.digits.iter().map(|d| (b'0' + d) as char).collect()
        };
        let e = if self.digits.is_empty() { 0 } else { self.exponent };
        let mut out = String::new();
        if self.negative {
            out.push('-');
        }
        if (0..=7).contains(&e) {
            if e == 0 {
                out.push_str("0.");
                out.extend(&digits);
            } else {
                let e = e as usize;
                for i in 0..e {
                    out.push(*digits.get(i).unwrap_or(&'0'));
                }
                out.push('.');
                if digits.len() > e {
                    out.extend(&digits[e..]);
                } else {
                    out.push('0');
                }
            }
        } else {
            out.push(digits[0]);
            out.push('.');
            if digits.len() > 1 {
                out.extend(&digits[1..]);
            } else {
                out.push('0');
            }
            out.push('e');
            out.push_str(&(e - 1).to_string());
        }
        out
    }
}

/// Parse JSON text as aeson does (`eitherDecode`), with its error messages.
fn parse_json(bytes: &[u8]) -> Result<Scalar, String> {
    let mut p = Parser { s: bytes, i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i < p.s.len() {
        return Err("Trailing garbage".to_owned());
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// `Unexpected "<next 30 bytes>", expecting <what>` (or end-of-input).
    fn unexpected(&self, expecting: &str) -> String {
        if self.i >= self.s.len() {
            return format!("Unexpected end-of-input, expecting {expecting}");
        }
        let end = (self.i + 30).min(self.s.len());
        format!(
            "Unexpected {}, expecting {expecting}",
            haskell_show(&self.s[self.i..end])
        )
    }

    fn value(&mut self) -> Result<Scalar, String> {
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                self.object()
            }
            Some(b'[') => {
                self.i += 1;
                self.array()
            }
            Some(b'"') => {
                self.i += 1;
                self.string().map(Scalar::String)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => {
                for (lit, v) in [
                    (&b"true"[..], Scalar::Bool(true)),
                    (b"false", Scalar::Bool(false)),
                    (b"null", Scalar::Null),
                ] {
                    if self.s[self.i..].starts_with(lit) {
                        self.i += lit.len();
                        return Ok(v);
                    }
                }
                Err(self.unexpected("JSON value"))
            }
        }
    }

    fn array(&mut self) -> Result<Scalar, String> {
        let mut items = vec![];
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Scalar::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Scalar::Array(items));
                }
                _ => return Err(self.unexpected(", or ]")),
            }
        }
    }

    fn object(&mut self) -> Result<Scalar, String> {
        let mut pairs = vec![];
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Scalar::Object(pairs));
        }
        let mut expecting = "record key literal or }";
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(self.unexpected(expecting));
            }
            self.i += 1;
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.unexpected(":"));
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            pairs.push((key, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Scalar::Object(pairs));
                }
                _ => return Err(self.unexpected(", or }")),
            }
            expecting = "record key literal";
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let start = self.i;
        let mut escaped = false;
        loop {
            match self.peek() {
                None => return Err("Unexpected end-of-input while parsing string literal".into()),
                Some(b'"') if !escaped => break,
                Some(b'\\') if !escaped => escaped = true,
                Some(_) => escaped = false,
            }
            self.i += 1;
        }
        let raw = &self.s[start..self.i];
        self.i += 1;
        let mut quoted = Vec::with_capacity(raw.len() + 2);
        quoted.push(b'"');
        quoted.extend_from_slice(raw);
        quoted.push(b'"');
        serde_json::from_slice::<String>(&quoted).map_err(|_| {
            "Cannot decode input: Data.Text.Internal.Encoding.decodeUtf8: Invalid UTF-8 stream"
                .to_owned()
        })
    }

    fn number(&mut self) -> Result<Scalar, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err("Number literal with leading zero".to_owned());
                }
            }
            Some(b'1'..=b'9') => self.digits(),
            Some(c) => return Err(format!("Unexpected {c} while parsing number literal")),
            None => return Err("Unexpected end-of-input while parsing number literal".into()),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            match self.peek() {
                Some(b'0'..=b'9') => self.digits(),
                Some(c) => return Err(format!("Unexpected {c} while parsing number literal")),
                None => {
                    return Err("Unexpected end-of-input while parsing number literal".into());
                }
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            match self.peek() {
                Some(b'0'..=b'9') => self.digits(),
                Some(c) => return Err(format!("Unexpected {c} while parsing number literal")),
                None => {
                    return Err("Unexpected end-of-input while parsing number literal".into());
                }
            }
        }
        let lit = std::str::from_utf8(&self.s[start..self.i]).expect("ascii");
        let lit = lit.replace("e+", "e").replace("E+", "e");
        Sci::parse(&lit)
            .map(Scalar::Number)
            .ok_or_else(|| "Number literal out of range".to_owned())
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
    }
}

/// Haskell's `show` of the bytes as a `String` of Latin-1 characters.
fn haskell_show(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                out.push('\\');
                out.push_str(&b.to_string());
                // `\&` separates a numeric escape from a following digit.
                if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    out.push_str("\\&");
                }
            }
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> Result<NodeId, String> {
        parse_node_id(&base64_decode_lenient(s))
    }

    #[test]
    fn decodes_ids() {
        assert_eq!(
            decode("WzEsICJwdWJsaWMiLCAiYXV0aG9yIiwgMl0K").unwrap(),
            NodeId {
                schema: "public".into(),
                table: "author".into(),
                values: vec![Scalar::Number(Sci::parse("2").unwrap())],
            }
        );
        // No padding.
        assert!(decode("WzEsInB1YmxpYyIsImF1dGhvciIsMV0").is_ok());
    }

    #[test]
    fn v2_error_messages() {
        let cases = [
            ("random", r#"Unexpected "\173\169\221\162", expecting JSON value"#),
            (
                "WzIsICJwdWJsaWMiLCAiYXV0aG9yIiwgMV0K",
                "Error in $: unsupported GUID version: 2.0",
            ),
            (
                "eyJpZCIgOiA0fQ==",
                "Error in $: parsing node id failed, expected Array, but encountered Object",
            ),
            (
                "WyIxIiwgInB1YmxpYyIsICJ1c2VyIiwgImZpcnN0XzIiLCAibGFzdF8yIiwgMl0K",
                "Error in $: unexpected GUID format, needs to start with a version number",
            ),
            (
                "WzEsICJwdWJsaWMiLCAidXNlciJdCg==",
                "Error in $: GUID version 1: expecting schema name, table name and at least one column value",
            ),
            ("", "Unexpected end-of-input, expecting JSON value"),
        ];
        for (id, message) in cases {
            assert_eq!(decode(id).unwrap_err(), message, "{id}");
        }
        let raw = |s: &str| parse_node_id(s.as_bytes()).unwrap_err();
        assert_eq!(raw("[1, x]"), r#"Unexpected "x]", expecting JSON value"#);
        assert_eq!(raw("[1]x"), "Trailing garbage");
        assert_eq!(raw("{"), "Unexpected end-of-input, expecting record key literal or }");
        assert_eq!(raw("[1,\"a\" 1]"), r#"Unexpected "1]", expecting , or ]"#);
        assert_eq!(raw("[1,-]"), "Unexpected 93 while parsing number literal");
        assert_eq!(raw("[01]"), "Number literal with leading zero");
        assert_eq!(raw("{\"a\" 1}"), r#"Unexpected "1}", expecting :"#);
        assert_eq!(
            raw("abcdefghijklmnopqrstuvwxyz0123456789"),
            r#"Unexpected "abcdefghijklmnopqrstuvwxyz0123", expecting JSON value"#
        );
    }

    #[test]
    fn shows_numbers_like_haskell() {
        let show = |s: &str| Sci::parse(s).unwrap().show();
        assert_eq!(show("2"), "2.0");
        assert_eq!(show("1.5"), "1.5");
        assert_eq!(show("99999999999"), "9.9999999999e10");
        assert_eq!(show("1e400"), "1.0e400");
        assert_eq!(show("0.05"), "5.0e-2");
        assert_eq!(show("0.5"), "0.5");
        assert_eq!(show("1234567"), "1234567.0");
        assert_eq!(show("0"), "0.0");
        assert_eq!(show("-3"), "-3.0");
    }

    #[test]
    fn column_values() {
        let int = ColumnType::builtin("int4");
        let text = ColumnType::builtin("text");
        let n = |s: &str| Scalar::Number(Sci::parse(s).unwrap());
        assert_eq!(column_value(&n("5"), &int).unwrap(), Some("5".into()));
        assert_eq!(
            column_value(&n("1.5"), &int).unwrap_err(),
            "The value 1.5 lies outside the bounds or is not an integer. Maybe it is a float, or is there integer overflow?"
        );
        assert_eq!(
            column_value(&Scalar::Bool(true), &int).unwrap_err(),
            "parsing Integer expected for input type: PGInteger failed, expected Number, but encountered Boolean"
        );
        assert_eq!(
            column_value(&n("1"), &text).unwrap_err(),
            "parsing Text failed, expected String, but encountered Number"
        );
        assert_eq!(
            column_value(&Scalar::String("x".into()), &int).unwrap(),
            Some("x".into())
        );
        assert_eq!(column_value(&Scalar::Null, &int).unwrap(), None);
    }
}
