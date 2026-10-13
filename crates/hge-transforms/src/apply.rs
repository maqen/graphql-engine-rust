//! Applying request transforms (§10.3, §10.4) and response transforms (§10.5).

use hge_core::Error;
use hge_kriti::{Bindings, Functions, get_session_variable_fn};
use serde_json::{Map, Value, json};

use crate::http::{Request, UrlTarget, encode_uri_component, parse_query, query_json, url_target};
use crate::metadata::{
    BodyTransform, HeadersTransform, QueryParamsTransform, RequestTransform, ResponseTransform,
    Tmpl,
};

/// A transform failure: V2's `TransformErrorBundle`, a list of Kriti errors
/// (`KritiError::to_json`) and `{"error_code":"TransformationError",…}` objects.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ErrorBundle(pub Vec<Value>);

impl ErrorBundle {
    /// The bundle as V2 serializes it: a JSON array.
    pub fn to_json(&self) -> Value {
        Value::Array(self.0.clone())
    }

    /// `500 unexpected "Request Transformation Failed"`, `internal` = the bundle (V2's
    /// `throw500WithDetail`).
    pub fn request_failed(self) -> Error {
        Error::unexpected("Request Transformation Failed").with_internal(Value::Array(self.0))
    }

    /// `500 unexpected "Response Transformation Failed"`, `internal` = the bundle.
    pub fn response_failed(self) -> Error {
        Error::unexpected("Response Transformation Failed").with_internal(Value::Array(self.0))
    }
}

impl From<Vec<Value>> for ErrorBundle {
    fn from(errors: Vec<Value>) -> Self {
        ErrorBundle(errors)
    }
}

/// A non-Kriti transform error.
pub fn transformation_error(message: impl Into<String>, value: Option<Value>) -> Value {
    let mut o = Map::new();
    o.insert("error_code".into(), "TransformationError".into());
    o.insert("message".into(), Value::String(message.into()));
    if let Some(v) = value {
        o.insert("value".into(), v);
    }
    Value::Object(o)
}

/// The variables a request transform sees (`RequestTransformCtx`), built from a request.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestContext {
    /// `$body`: the body decoded as JSON, `null` if it is not JSON.
    pub body: Value,
    /// `$session_variables`: an object with lower-cased keys, or `null`.
    pub session_variables: Value,
    /// `$query_params`: `[[key, value-or-null], …]`.
    pub query_params: Value,
    /// `$base_url`: the webhook URL as resolved (not normalized).
    pub base_url: Option<String>,
}

impl RequestContext {
    /// V2's `mkReqTransformCtx`. Callers that hold the body as JSON already can build the
    /// context from its fields instead (see [`RequestContext::from_parts`]).
    pub fn new(
        request: &Request,
        base_url: Option<&str>,
        session_variables: Option<Value>,
    ) -> Self {
        RequestContext {
            body: serde_json::from_slice(&request.body).unwrap_or(Value::Null),
            session_variables: session_variables.unwrap_or(Value::Null),
            query_params: request.query_json(),
            base_url: base_url.map(str::to_owned),
        }
    }

    /// The context of a request whose body is already decoded.
    pub fn from_parts(
        body: Value,
        query: &[(String, Option<String>)],
        base_url: Option<&str>,
        session_variables: Option<Value>,
    ) -> Self {
        RequestContext {
            body,
            session_variables: session_variables.unwrap_or(Value::Null),
            query_params: query_json(query),
            base_url: base_url.map(str::to_owned),
        }
    }

    /// `ToJSON RequestTransformCtx`.
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        if let Some(u) = &self.base_url {
            o.insert("base_url".into(), u.clone().into());
        }
        o.insert("body".into(), self.body.clone());
        o.insert("query_params".into(), self.query_params.clone());
        o.insert("session_variables".into(), self.session_variables.clone());
        Value::Object(o)
    }

    /// The `$request` of a response transform: V2 serializes its per-field request context
    /// with the same context object under every field.
    pub fn to_response_json(&self) -> Value {
        let c = self.to_json();
        json!({
            "body": c,
            "method": c,
            "query_params": c,
            "request_headers": c,
            "url": c,
        })
    }

    fn into_bindings(self) -> (Bindings, Functions) {
        let funcs = functions(&self.session_variables);
        let mut b = Bindings::new()
            .with("$body", self.body)
            .with("$session_variables", self.session_variables)
            .with("$query_params", self.query_params);
        if let Some(u) = self.base_url {
            b.insert("$base_url", u.into());
        }
        (b, funcs)
    }
}

fn functions(session_variables: &Value) -> Functions {
    Functions::new().with(
        "getSessionVariable",
        get_session_variable_fn(session_variables.as_object().cloned()),
    )
}

/// Evaluates a template; a parse error kept from metadata loading is reported here.
fn eval(t: &Tmpl, vars: &Bindings, funcs: &Functions) -> Result<Value, Value> {
    match t.parsed() {
        Ok(t) => t.eval(vars, funcs).map_err(|e| e.to_json()),
        Err(e) => Err(e.to_json()),
    }
}

/// Evaluates an unescaped template to text (V2's `encodeScalar`).
fn eval_text(t: &Tmpl, vars: &Bindings, funcs: &Functions) -> Result<String, Value> {
    match eval(t, vars, funcs)? {
        Value::String(s) => Ok(s),
        v @ (Value::Number(_) | Value::Bool(_)) => Ok(v.to_string()),
        v => Err(transformation_error(
            "Template must produce a String, Number, or Boolean value",
            Some(v),
        )),
    }
}

/// Collects every error of `items` (V2's applicative `Validation`).
fn collect<T>(items: impl IntoIterator<Item = Result<T, Value>>) -> Result<Vec<T>, ErrorBundle> {
    let mut ok = vec![];
    let mut errors = vec![];
    for item in items {
        match item {
            Ok(v) => ok.push(v),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(ok)
    } else {
        Err(ErrorBundle(errors))
    }
}

/// `x_www_form_urlencoded`: `k=v` pairs in key order, dropping values that are `null`.
fn form_body(
    form: &[(String, Tmpl)],
    vars: &Bindings,
    funcs: &Functions,
) -> Result<String, ErrorBundle> {
    let mut sorted: Vec<&(String, Tmpl)> = form.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let pairs = collect(
        sorted
            .into_iter()
            .map(|(k, t)| eval_text(t, vars, funcs).map(|v| (k, v))),
    )?;
    Ok(pairs
        .into_iter()
        .filter(|(_, v)| v != "null")
        .map(|(k, v)| format!("{}={}", encode_uri_component(k), encode_uri_component(&v)))
        .collect::<Vec<_>>()
        .join("&"))
}

fn url(t: &Tmpl, vars: &Bindings, funcs: &Functions) -> Result<String, ErrorBundle> {
    let one = |e| ErrorBundle(vec![e]);
    match eval(t, vars, funcs).map_err(one)? {
        Value::String(u) if url_target(&u) != UrlTarget::Invalid => Ok(u),
        Value::String(u) => Err(one(transformation_error(format!("Invalid URL: {u}"), None))),
        v => Err(one(transformation_error(
            format!("URL Transforms must produce a JSON String: {v}"),
            None,
        ))),
    }
}

fn request_body(
    b: &BodyTransform,
    vars: &Bindings,
    funcs: &Functions,
) -> Result<Vec<u8>, ErrorBundle> {
    match b {
        BodyTransform::Remove => Ok(vec![]),
        // Kriti results have sorted keys, so this is V2's sorted-key encoding.
        BodyTransform::Transform(t) => eval(t, vars, funcs)
            .map(|v| v.to_string().into_bytes())
            .map_err(|e| ErrorBundle(vec![e])),
        BodyTransform::FormUrlEncoded(form) => form_body(form, vars, funcs).map(String::into_bytes),
    }
}

type Query = Vec<(String, Option<String>)>;

fn query_params(
    q: &QueryParamsTransform,
    vars: &Bindings,
    funcs: &Functions,
) -> Result<Query, ErrorBundle> {
    match q {
        QueryParamsTransform::Template(t) => eval_text(t, vars, funcs)
            .map(|s| parse_query(&s))
            .map_err(|e| ErrorBundle(vec![e])),
        QueryParamsTransform::AddOrReplace(params) => {
            let mut out: Query = vec![];
            let mut errors = vec![];
            for (k, v) in params {
                let k = eval_text(k, vars, funcs);
                let v = v.as_ref().map(|v| eval_text(v, vars, funcs)).transpose();
                match (k, v) {
                    (Ok(k), Ok(v)) => {
                        if k != "null" && v.as_deref() != Some("null") {
                            out.push((k, v));
                        }
                    }
                    (k, v) => errors.extend(k.err().into_iter().chain(v.err())),
                }
            }
            if !errors.is_empty() {
                return Err(ErrorBundle(errors));
            }
            // V2 emits HashMap order; sort by the evaluated key for a stable order.
            out.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(out)
        }
    }
}

/// `true` if `name` is an HTTP header name and `value` a header value (no CR/LF/NUL).
fn valid_header(name: &str, value: &str) -> bool {
    http::HeaderName::from_bytes(name.as_bytes()).is_ok()
        && http::HeaderValue::from_str(value).is_ok()
}

fn headers(
    h: &HeadersTransform,
    original: Vec<(String, String)>,
    vars: &Bindings,
    funcs: &Functions,
) -> Result<Vec<(String, String)>, ErrorBundle> {
    let added = collect(h.add_headers.iter().map(|(k, t)| {
        let v = eval_text(t, vars, funcs)?;
        if !valid_header(k, &v) {
            return Err(transformation_error(
                format!("Invalid header: {k}"),
                Some(Value::String(v)),
            ));
        }
        Ok((k.clone(), v))
    }))?;
    Ok(original
        .into_iter()
        .filter(|(k, _)| !h.remove_headers.iter().any(|r| r.eq_ignore_ascii_case(k)))
        .chain(added)
        .collect())
}

fn field<T>(r: Option<Result<T, ErrorBundle>>, errors: &mut Vec<Value>) -> Option<T> {
    match r? {
        Ok(v) => Some(v),
        Err(e) => {
            errors.extend(e.0);
            None
        }
    }
}

/// Applies a request transform. Every field is computed from the original request's
/// context; all errors are collected into one bundle. The request is consumed so that
/// replaced parts are not copied.
///
/// The query string is always rewritten after the URL (V2 quirk, §10.4): it is the
/// `query_params` result if there is one, else the original request's query parameters,
/// so a query string produced by a URL transform is dropped. A URL with userinfo is
/// rejected (see [`UrlTarget::Invalid`]), and so are a method or added header that cannot
/// be sent.
pub fn apply_request_transform(
    t: &RequestTransform,
    mut request: Request,
    ctx: RequestContext,
) -> Result<Request, ErrorBundle> {
    let (vars, funcs) = ctx.into_bindings();
    let mut errors = vec![];
    if let Some(m) = &t.method
        && http::Method::from_bytes(m.as_bytes()).is_err()
    {
        errors.push(transformation_error(
            "Invalid HTTP method",
            Some(Value::String(m.clone())),
        ));
    }
    let new_url = field(t.url.as_ref().map(|u| url(u, &vars, &funcs)), &mut errors);
    let body = field(
        t.body.as_ref().map(|b| request_body(b, &vars, &funcs)),
        &mut errors,
    );
    let query = field(
        t.query_params
            .as_ref()
            .map(|q| query_params(q, &vars, &funcs)),
        &mut errors,
    );
    let original_headers = std::mem::take(&mut request.headers);
    let headers = match &t.request_headers {
        Some(h) => field(
            Some(headers(h, original_headers, &vars, &funcs)),
            &mut errors,
        ),
        None => Some(original_headers),
    };
    if !errors.is_empty() {
        return Err(ErrorBundle(errors));
    }
    if let Some(m) = &t.method {
        request.method = m.clone();
    }
    if let Some(b) = body {
        request.body = b;
    }
    let original_query = std::mem::take(&mut request.query);
    if let Some(u) = new_url {
        request.set_url(&u);
    }
    request.query = query.unwrap_or(original_query);
    request.headers = headers.unwrap_or_default();
    Ok(request)
}

/// Recursively sorts object keys (aeson encodes through a `Map`).
fn sorted(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// The variables a response transform sees besides the response body.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseContext {
    /// `$request`: `null` without a request transform, else
    /// [`RequestContext::to_response_json`] of the transformed request.
    pub request: Value,
    /// `$response.status`.
    pub status: u16,
    /// `$session_variables` (and `getSessionVariable`).
    pub session_variables: Value,
}

/// Applies a response transform to the raw response body. The result is the new body as
/// JSON (V2 re-encodes it; callers that need bytes encode it themselves).
pub fn apply_response_transform(
    t: &ResponseTransform,
    body: &[u8],
    ctx: ResponseContext,
) -> Result<Value, ErrorBundle> {
    let decoded: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let template = match &t.body {
        None => return Ok(sorted(decoded)),
        Some(BodyTransform::Remove) => return Ok(Value::Null),
        Some(b) => b,
    };
    let funcs = functions(&ctx.session_variables);
    let vars = Bindings::new()
        .with("$request", ctx.request)
        .with("$response", json!({"status": ctx.status}))
        .with("$session_variables", ctx.session_variables)
        .with("$body", decoded);
    match template {
        BodyTransform::Transform(t) => eval(t, &vars, &funcs).map_err(|e| ErrorBundle(vec![e])),
        BodyTransform::FormUrlEncoded(form) => form_body(form, &vars, &funcs).map(Value::from),
        BodyTransform::Remove => Ok(Value::Null),
    }
}
