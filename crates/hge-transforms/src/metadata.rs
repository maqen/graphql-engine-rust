//! The metadata JSON of `request_transform` / `response_transform` (spec §10.1): parsing
//! (versions 1 and 2), the V2 `ToJSON` round trip, and metadata-time validation (§10.2).

use hge_core::{Code, Error};
use hge_kriti::{KritiError, Template};
use serde_json::{Map, Value};

/// `version`: 1 (body is a template string) or 2 (body is a tagged object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

impl Version {
    fn to_json(self) -> Value {
        match self {
            Version::V1 => 1.into(),
            Version::V2 => 2.into(),
        }
    }
}

/// A Kriti template from metadata. It is parsed once, when the metadata is read; a parse
/// error is kept and reported when the transform runs (V2 only parse-checks templates in
/// the metadata API commands, §10.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Tmpl {
    source: String,
    parsed: Result<Template, KritiError>,
}

impl Tmpl {
    /// A full Kriti program (body templates).
    pub fn program(source: &str) -> Tmpl {
        Tmpl {
            source: source.to_owned(),
            parsed: Template::parse(source),
        }
    }

    /// An "unescaped" template (urls, headers, query params, form fields), evaluated as the
    /// program `"` + source + `"`.
    pub fn string(source: &str) -> Tmpl {
        Tmpl {
            source: source.to_owned(),
            parsed: Template::parse_string_template(source),
        }
    }

    /// The template as written in the metadata.
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn parsed(&self) -> Result<&Template, &KritiError> {
        self.parsed.as_ref()
    }
}

/// A body transform (`BodyTransformFn`).
#[derive(Debug, Clone, PartialEq)]
pub enum BodyTransform {
    /// `{"action": "remove"}`.
    Remove,
    /// `{"action": "transform", "template": …}` (or a v1 template string).
    Transform(Tmpl),
    /// `{"action": "x_www_form_urlencoded", "form_template": {k: template}}`.
    FormUrlEncoded(Vec<(String, Tmpl)>),
}

/// `query_params`: an object of key/value templates or one query-string template.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryParamsTransform {
    /// `{k: template | null}`; both keys and values are unescaped templates.
    AddOrReplace(Vec<(Tmpl, Option<Tmpl>)>),
    /// A template producing a query string.
    Template(Tmpl),
}

/// `request_headers`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HeadersTransform {
    pub add_headers: Vec<(String, Tmpl)>,
    pub remove_headers: Vec<String>,
}

/// `request_transform` (`RequestTransform`). The template engine is always Kriti.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestTransform {
    pub version: Version,
    /// Replacement method, as written (not validated).
    pub method: Option<String>,
    pub url: Option<Tmpl>,
    pub query_params: Option<QueryParamsTransform>,
    pub request_headers: Option<HeadersTransform>,
    pub body: Option<BodyTransform>,
}

/// `response_transform` (`MetadataResponseTransform`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseTransform {
    pub version: Version,
    pub body: Option<BodyTransform>,
}

/// A metadata decoding failure, aeson style: `Error in $.body: invalid transform action`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// JSON path relative to the transform object (`$`, `$.body.action`).
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Error in {}: {}", self.path, self.message)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(path: &str, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        path: path.to_owned(),
        message: message.into(),
    })
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

fn expected<T>(path: &str, what: &str, v: &Value) -> Result<T, ParseError> {
    err(
        path,
        format!(
            "parsing {what} failed, expected {}, but encountered {}",
            if what == "Text" { "String" } else { what },
            type_name(v)
        ),
    )
}

fn object<'a>(path: &str, v: &'a Value) -> Result<&'a Map<String, Value>, ParseError> {
    v.as_object()
        .map_or_else(|| expected(path, "Object", v), Ok)
}

fn string<'a>(path: &str, v: &'a Value) -> Result<&'a str, ParseError> {
    v.as_str().map_or_else(|| expected(path, "Text", v), Ok)
}

/// An optional key: absent and `null` are the same (aeson's `.:?`).
fn opt<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

fn version(o: &Map<String, Value>) -> Result<Version, ParseError> {
    let Some(v) = opt(o, "version") else {
        return Ok(Version::V1);
    };
    // aeson reads an `Int` from any integral number (`2.0` included).
    let n = v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.fract() == 0.0 && f.abs() < 9e15)
            .map(|f| f as i64)
    });
    match n {
        Some(1) => Ok(Version::V1),
        Some(2) => Ok(Version::V2),
        Some(n) => err("$.version", format!("expected 1 or 2, encountered {n}")),
        None => expected("$.version", "Int", v),
    }
}

/// An object with keys in sorted order (V2 encodes its HashMaps through aeson's `Map`).
fn sorted_object<'a>(entries: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    let mut entries: Vec<(&str, Value)> = entries.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

fn template_engine(o: &Map<String, Value>) -> Result<(), ParseError> {
    match opt(o, "template_engine") {
        None => Ok(()),
        Some(Value::String(s)) if s == "Kriti" => Ok(()),
        Some(Value::String(s)) => err(
            "$.template_engine",
            format!(
                "parsing TemplatingEngine failed, expected one of the tags ['Kriti'], but found tag '{s}'"
            ),
        ),
        Some(v) => expected("$.template_engine", "Text", v),
    }
}

fn form_template(path: &str, v: &Value) -> Result<Vec<(String, Tmpl)>, ParseError> {
    object(path, v)?
        .iter()
        .map(|(k, t)| Ok((k.clone(), Tmpl::string(string(&format!("{path}.{k}"), t)?))))
        .collect()
}

fn body(version: Version, v: &Value) -> Result<BodyTransform, ParseError> {
    if version == Version::V1 {
        return Ok(BodyTransform::Transform(Tmpl::program(string(
            "$.body", v,
        )?)));
    }
    let o = object("$.body", v)?;
    let Some(action) = o.get("action") else {
        return err("$.body", "key \"action\" not found");
    };
    match string("$.body.action", action)? {
        "remove" => Ok(BodyTransform::Remove),
        "transform" => match o.get("template") {
            Some(t) => Ok(BodyTransform::Transform(Tmpl::program(string(
                "$.body.template",
                t,
            )?))),
            None => err("$.body", "key \"template\" not found"),
        },
        "x_www_form_urlencoded" => match o.get("form_template") {
            Some(f) => Ok(BodyTransform::FormUrlEncoded(form_template(
                "$.body.form_template",
                f,
            )?)),
            None => err("$.body", "key \"form_template\" not found"),
        },
        _ => err("$.body", "invalid transform action"),
    }
}

fn body_to_json(version: Version, body: &BodyTransform) -> Option<Value> {
    let form = |f: &[(String, Tmpl)]| {
        sorted_object(f.iter().map(|(k, t)| (k.as_str(), t.source.clone().into())))
    };
    let mut o = Map::new();
    match (version, body) {
        (Version::V1, BodyTransform::Transform(t)) => return Some(t.source.clone().into()),
        // v1 can only represent a transform body.
        (Version::V1, _) => return None,
        (Version::V2, BodyTransform::Remove) => {
            o.insert("action".into(), "remove".into());
        }
        (Version::V2, BodyTransform::Transform(t)) => {
            o.insert("action".into(), "transform".into());
            o.insert("template".into(), t.source.clone().into());
        }
        (Version::V2, BodyTransform::FormUrlEncoded(f)) => {
            o.insert("action".into(), "x_www_form_urlencoded".into());
            o.insert("form_template".into(), form(f));
        }
    }
    Some(Value::Object(o))
}

fn query_params(v: &Value) -> Result<QueryParamsTransform, ParseError> {
    match v {
        Value::Object(o) => o
            .iter()
            .map(|(k, val)| {
                let val = match val {
                    Value::Null => None,
                    other => Some(Tmpl::string(string(&format!("$.query_params.{k}"), other)?)),
                };
                Ok((Tmpl::string(k), val))
            })
            .collect::<Result<_, _>>()
            .map(QueryParamsTransform::AddOrReplace),
        Value::String(s) => Ok(QueryParamsTransform::Template(Tmpl::string(s))),
        _ => err("$.query_params", "Invalid query parameter"),
    }
}

fn request_headers(v: &Value) -> Result<HeadersTransform, ParseError> {
    let o = object("$.request_headers", v)?;
    let add_headers = match opt(o, "add_headers") {
        None => vec![],
        Some(a) => object("$.request_headers.add_headers", a)?
            .iter()
            .map(|(k, t)| {
                let path = format!("$.request_headers.add_headers.{k}");
                Ok((k.clone(), Tmpl::string(string(&path, t)?)))
            })
            .collect::<Result<_, _>>()?,
    };
    let remove_headers = match opt(o, "remove_headers") {
        None => vec![],
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, h)| {
                string(&format!("$.request_headers.remove_headers[{i}]"), h).map(str::to_owned)
            })
            .collect::<Result<_, _>>()?,
        Some(other) => return expected("$.request_headers.remove_headers", "Array", other),
    };
    Ok(HeadersTransform {
        add_headers,
        remove_headers,
    })
}

impl RequestTransform {
    /// Parses a `request_transform` object (unknown keys are ignored).
    pub fn from_json(v: &Value) -> Result<RequestTransform, ParseError> {
        let o = object("$", v)?;
        let version = version(o)?;
        template_engine(o)?;
        Ok(RequestTransform {
            version,
            method: opt(o, "method")
                .map(|m| string("$.method", m).map(str::to_owned))
                .transpose()?,
            url: opt(o, "url")
                .map(|u| string("$.url", u).map(Tmpl::string))
                .transpose()?,
            query_params: opt(o, "query_params").map(query_params).transpose()?,
            request_headers: opt(o, "request_headers").map(request_headers).transpose()?,
            body: opt(o, "body").map(|b| body(version, b)).transpose()?,
        })
    }

    /// V2's `ToJSON RequestTransform`: `version` and `template_engine` are always emitted,
    /// `request_headers` always has both lists.
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("version".into(), self.version.to_json());
        o.insert("template_engine".into(), "Kriti".into());
        if let Some(m) = &self.method {
            o.insert("method".into(), m.clone().into());
        }
        if let Some(u) = &self.url {
            o.insert("url".into(), u.source.clone().into());
        }
        match &self.query_params {
            None => {}
            Some(QueryParamsTransform::Template(t)) => {
                o.insert("query_params".into(), t.source.clone().into());
            }
            Some(QueryParamsTransform::AddOrReplace(params)) => {
                let params = sorted_object(params.iter().map(|(k, v)| {
                    let v = v.as_ref().map_or(Value::Null, |v| v.source.clone().into());
                    (k.source.as_str(), v)
                }));
                o.insert("query_params".into(), params);
            }
        }
        if let Some(h) = &self.request_headers {
            let add = sorted_object(
                h.add_headers
                    .iter()
                    .map(|(k, t)| (k.as_str(), t.source.clone().into())),
            );
            let mut headers = Map::new();
            headers.insert("add_headers".into(), add);
            headers.insert(
                "remove_headers".into(),
                h.remove_headers.iter().cloned().map(Value::from).collect(),
            );
            o.insert("request_headers".into(), Value::Object(headers));
        }
        if let Some(b) = self
            .body
            .as_ref()
            .and_then(|b| body_to_json(self.version, b))
        {
            o.insert("body".into(), b);
        }
        Value::Object(o)
    }

    /// Every template of the transform, in field order (url, body, query params, headers).
    fn templates(&self) -> Vec<&Tmpl> {
        let mut out: Vec<&Tmpl> = self.url.iter().collect();
        if let Some(b) = &self.body {
            out.extend(body_templates(b));
        }
        match &self.query_params {
            None => {}
            Some(QueryParamsTransform::Template(t)) => out.push(t),
            Some(QueryParamsTransform::AddOrReplace(params)) => {
                for (k, v) in params {
                    out.push(k);
                    out.extend(v);
                }
            }
        }
        if let Some(h) = &self.request_headers {
            out.extend(h.add_headers.iter().map(|(_, t)| t));
        }
        out
    }
}

fn body_templates(b: &BodyTransform) -> Vec<&Tmpl> {
    match b {
        BodyTransform::Remove => vec![],
        BodyTransform::Transform(t) => vec![t],
        BodyTransform::FormUrlEncoded(f) => f.iter().map(|(_, t)| t).collect(),
    }
}

impl ResponseTransform {
    /// Parses a `response_transform` object (unknown keys are ignored).
    pub fn from_json(v: &Value) -> Result<ResponseTransform, ParseError> {
        let o = object("$", v)?;
        template_engine(o)?;
        let version = version(o)?;
        Ok(ResponseTransform {
            version,
            body: opt(o, "body").map(|b| body(version, b)).transpose()?,
        })
    }

    /// V2's `ToJSON MetadataResponseTransform`: `template_engine`, `version`, then `body`.
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("template_engine".into(), "Kriti".into());
        o.insert("version".into(), self.version.to_json());
        if let Some(b) = self
            .body
            .as_ref()
            .and_then(|b| body_to_json(self.version, b))
        {
            o.insert("body".into(), b);
        }
        Value::Object(o)
    }
}

fn parse_errors<'a>(templates: impl IntoIterator<Item = &'a Tmpl>) -> Vec<Value> {
    templates
        .into_iter()
        .filter_map(|t| t.parsed.as_ref().err().map(KritiError::to_json))
        .collect()
}

/// Parse-checks every template of a request transform (§10.2): `400 validation-failed`
/// `request transform validation failed`, `internal` = the Kriti errors.
pub fn validate_request_transform(t: &RequestTransform) -> Result<(), Error> {
    let errors = parse_errors(t.templates());
    if errors.is_empty() {
        return Ok(());
    }
    Err(Error::validation("request transform validation failed").with_internal(errors.into()))
}

/// Parse-checks every template of a response transform (§10.2).
pub fn validate_response_transform(t: &ResponseTransform) -> Result<(), Error> {
    let errors = parse_errors(t.body.iter().flat_map(body_templates));
    if errors.is_empty() {
        return Ok(());
    }
    Err(Error::validation("response transform validation failed").with_internal(errors.into()))
}

/// V2's `validateTransforms`, run by `create_action`, `update_action`, event/cron trigger
/// creation and `test_webhook_transform`: the response transform first, then the request
/// transform.
pub fn validate_transforms(
    request: Option<&RequestTransform>,
    response: Option<&ResponseTransform>,
) -> Result<(), Error> {
    if let Some(r) = response {
        validate_response_transform(r)?;
    }
    if let Some(r) = request {
        validate_request_transform(r)?;
    }
    Ok(())
}

/// Parses an optional transform from a metadata object, as `Error in $.<key>…` messages.
pub fn parse_optional<T>(
    v: Option<&Value>,
    key: &str,
    parse: impl Fn(&Value) -> Result<T, ParseError>,
) -> Result<Option<T>, Error> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse(v).map(Some).map_err(|e| {
            Error::new(Code::ParseFailed, e.message).with_path(format!(
                "$.{key}{}",
                e.path.strip_prefix('$').unwrap_or(&e.path)
            ))
        }),
    }
}
