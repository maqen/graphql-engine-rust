//! A YAML document tree with go-yaml v3's node semantics, and the conversion from server
//! JSON that the official CLI does through CUE (`metadatautil.JSONToYAML`).

use super::emitter::{Event, ScalarStyle, emit};
use super::resolve::resolves_to_str;
use crate::json::Json;

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Scalar(Scalar),
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scalar {
    pub value: String,
    /// `!!str` scalars. Everything else (numbers, booleans, null) is written as-is.
    pub is_str: bool,
    /// The style the node asks for; `Plain` lets the encoder decide.
    pub style: ScalarStyle,
}

impl Node {
    /// A string as go-yaml encodes a Go `string` value (`encoder.stringv`).
    pub fn string(s: impl Into<String>) -> Node {
        let s = s.into();
        let can_use_plain = resolves_to_str(&s) && !is_base60_float(&s) && !is_old_bool(&s);
        let style = if s.contains('\n') {
            ScalarStyle::Literal
        } else if can_use_plain {
            ScalarStyle::Plain
        } else {
            ScalarStyle::DoubleQuoted
        };
        Node::Scalar(Scalar {
            value: s,
            is_str: true,
            style: ScalarStyle::Any,
        })
        .with_style(style)
    }

    /// A `!!str` node with an explicit style, like a `yaml.Node{Style: ...}`.
    pub fn styled(s: impl Into<String>, style: ScalarStyle) -> Node {
        Node::Scalar(Scalar {
            value: s.into(),
            is_str: true,
            style,
        })
    }

    /// A non-string scalar written verbatim (numbers, `true`, `null`).
    pub fn raw(s: impl Into<String>) -> Node {
        Node::Scalar(Scalar {
            value: s.into(),
            is_str: false,
            style: ScalarStyle::Plain,
        })
    }

    pub fn bool(b: bool) -> Node {
        Node::raw(if b { "true" } else { "false" })
    }

    fn with_style(self, style: ScalarStyle) -> Node {
        match self {
            Node::Scalar(mut s) => {
                s.style = style;
                Node::Scalar(s)
            }
            n => n,
        }
    }

    /// Convert server JSON the way `JSONToYAML` does: CUE quotes YAML 1.1 look-alikes
    /// (`yes`, `on`, dates, hex), multi-line strings become literal blocks, and go-yaml
    /// double-quotes any other string that would not read back as a string.
    pub fn from_json(v: &Json) -> Node {
        Node::from_json_in(v, false)
    }

    fn from_json_in(v: &Json, in_list: bool) -> Node {
        match v {
            Json::Null => Node::raw("null"),
            Json::Bool(b) => Node::bool(*b),
            Json::Number(n) => Node::raw(cue_number(n, in_list)),
            Json::String(s) => cue_string(s),
            Json::Array(a) => Node::Seq(a.iter().map(|v| Node::from_json_in(v, true)).collect()),
            Json::Object(o) => Node::Map(
                o.iter()
                    .map(|(k, v)| (cue_string(k), Node::from_json_in(v, false)))
                    .collect(),
            ),
        }
    }

    /// Back to JSON (for `!!str` nodes a string, otherwise the YAML 1.2 value).
    pub fn to_json(&self) -> Json {
        match self {
            Node::Scalar(s) if s.is_str => Json::String(s.value.clone()),
            Node::Scalar(s) => match s.value.as_str() {
                "null" | "~" | "" => Json::Null,
                "true" => Json::Bool(true),
                "false" => Json::Bool(false),
                v => Json::Number(v.to_string()),
            },
            Node::Seq(items) => Json::Array(items.iter().map(Node::to_json).collect()),
            Node::Map(fields) => Json::Object(
                fields
                    .iter()
                    .map(|(k, v)| {
                        let key = match k {
                            Node::Scalar(s) => s.value.clone(),
                            other => other.to_json().to_compact(),
                        };
                        (key, v.to_json())
                    })
                    .collect(),
            ),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(fields) => fields
                .iter()
                .find(|(k, _)| matches!(k, Node::Scalar(s) if s.value == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(&s.value),
            _ => None,
        }
    }

    /// Encode as a YAML document with `yaml.NewEncoder` + `SetIndent(2)`.
    pub fn to_yaml(&self) -> String {
        let mut events = vec![Event::StreamStart, Event::DocumentStart { implicit: true }];
        self.events(&mut events);
        events.push(Event::DocumentEnd { implicit: true });
        events.push(Event::StreamEnd);
        emit(&events, 2)
    }

    fn events(&self, out: &mut Vec<Event>) {
        match self {
            Node::Scalar(s) => {
                // encoder.node(): a string tag is dropped; a plain string that would
                // resolve to another type is forced into double quotes.
                let mut style = s.style;
                if s.is_str && matches!(style, ScalarStyle::Plain | ScalarStyle::Any) {
                    style = if s.value.contains('\n') {
                        ScalarStyle::Literal
                    } else if !resolves_to_str(&s.value) {
                        ScalarStyle::DoubleQuoted
                    } else {
                        ScalarStyle::Plain
                    };
                }
                out.push(Event::Scalar {
                    tag: String::new(),
                    value: s.value.clone(),
                    implicit: true,
                    quoted_implicit: true,
                    style,
                });
            }
            Node::Seq(items) => {
                out.push(Event::SequenceStart {
                    tag: String::new(),
                    implicit: true,
                    flow: false,
                });
                for i in items {
                    i.events(out);
                }
                out.push(Event::SequenceEnd);
            }
            Node::Map(fields) => {
                out.push(Event::MappingStart {
                    tag: String::new(),
                    implicit: true,
                    flow: false,
                });
                for (k, v) in fields {
                    k.events(out);
                    v.events(out);
                }
                out.push(Event::MappingEnd);
            }
        }
    }
}

/// How a JSON number comes out of CUE: positive list elements keep their text (with a
/// lower-case exponent), everything else is the evaluated decimal in apd's notation.
fn cue_number(text: &str, in_list: bool) -> String {
    let negative = text.starts_with('-');
    if in_list && !negative {
        return text.replace('E', "e");
    }
    let is_float = text.contains(['.', 'e', 'E']);
    let body = text.trim_start_matches('-');
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (int_part, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits_all = format!("{int_part}{frac}");
    let mut exponent = exp - frac.len() as i64;
    let mut coeff = digits_all.trim_start_matches('0').to_string();
    if coeff.is_empty() {
        coeff = "0".into();
    }
    if !is_float {
        exponent = 0;
    }
    let zero = coeff == "0";
    let mut out = String::new();
    if negative && !zero {
        out.push('-');
    }
    let adjusted = exponent + coeff.len() as i64 - 1;
    if exponent <= 0 && adjusted >= -6 {
        if exponent == 0 {
            out.push_str(&coeff);
        } else {
            let point = coeff.len() as i64 + exponent;
            if point > 0 {
                out.push_str(&coeff[..point as usize]);
                out.push('.');
                out.push_str(&coeff[point as usize..]);
            } else {
                out.push_str("0.");
                out.push_str(&"0".repeat((-point) as usize));
                out.push_str(&coeff);
            }
        }
    } else {
        out.push_str(&coeff[..1]);
        if coeff.len() > 1 {
            out.push('.');
            out.push_str(&coeff[1..]);
        }
        out.push('E');
        out.push(if adjusted < 0 { '-' } else { '+' });
        out.push_str(&adjusted.abs().to_string());
    }
    if is_float && !out.contains(['.', 'E']) {
        out.push('.');
    }
    out
}

fn cue_string(s: &str) -> Node {
    let style = if s.contains('\n') {
        ScalarStyle::Literal
    } else if cue_should_quote(s) {
        ScalarStyle::DoubleQuoted
    } else {
        ScalarStyle::Plain
    };
    Node::styled(s, style)
}

/// CUE's `shouldQuote`: YAML 1.1 legacy words, dates/times/base-60 numbers and hex.
fn cue_should_quote(s: &str) -> bool {
    matches!(
        s,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "n"
            | "N"
            | "t"
            | "T"
            | "f"
            | "F"
            | "no"
            | "No"
            | "NO"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
            | ".Nan"
    ) || cue_time_like(s)
        || s.strip_prefix("0x")
            .is_some_and(|h| !h.is_empty() && h.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// `^[\-+0-9:\. \t]+([-:]|[tT])[\-+0-9:\. \t]+[zZ]?$`
fn cue_time_like(s: &str) -> bool {
    let b = s.as_bytes();
    let class = |c: u8| matches!(c, b'-' | b'+' | b'0'..=b'9' | b':' | b'.' | b' ' | b'\t');
    let check = |body: &[u8]| {
        (1..body.len().saturating_sub(1)).any(|mid| {
            matches!(body[mid], b'-' | b':' | b't' | b'T')
                && body[..mid].iter().all(|&c| class(c))
                && body[mid + 1..].iter().all(|&c| class(c))
        })
    };
    check(b) || (matches!(b.last(), Some(b'z' | b'Z')) && check(&b[..b.len() - 1]))
}

fn is_old_bool(s: &str) -> bool {
    matches!(
        s,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "on"
            | "On"
            | "ON"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "off"
            | "Off"
            | "OFF"
    )
}

/// `^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?$`
fn is_base60_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    if !b.get(i).is_some_and(u8::is_ascii_digit) || !s.contains(':') {
        return false;
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    let mut groups = 0;
    while b.get(i) == Some(&b':') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() && i - start < 2 {
            i += 1;
        }
        match i - start {
            1 => {}
            2 if b[start] <= b'5' => {}
            _ => return false,
        }
        groups += 1;
    }
    if groups == 0 {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            i += 1;
        }
    }
    i == b.len()
}
