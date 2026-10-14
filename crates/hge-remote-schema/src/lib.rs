//! Remote schemas over HTTP: introspecting remote GraphQL servers and forwarding remote
//! root fields to them (spec 08 §3, §6.4-§6.5). The schema side (merging, lowering to
//! outgoing requests) lives in `hge_schema::remote`.

mod env;

use std::sync::Arc;
use std::time::Duration;

use hge_core::{Code, Error, Session};
use hge_ir::RemoteCall;
use hge_schema::remote::introspection::introspection_request_body;
use hge_schema::remote::metadata::{HeaderConf, RemoteSchemaDef};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value as Json, json};

pub use env::{EnvLookup, process_env, resolve_template};

/// Engine settings that change how remote responses are handled.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteSettings {
    /// `HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA`: when a remote returns both `data` and
    /// `errors`, use the data.
    pub prioritize_data: bool,
}

/// What a request carries to remote servers.
pub struct RequestContext<'a> {
    pub session: &'a Session,
    pub client_headers: &'a HeaderMap,
}

/// A failed remote root field.
#[derive(Debug, Clone)]
pub enum RemoteError {
    /// A Hasura error (HTTP failure, malformed response, …).
    Hasura(Error),
    /// The remote's GraphQL errors, returned to the client verbatim with `"data": null`.
    Graphql(Vec<Json>),
}

impl From<Error> for RemoteError {
    fn from(e: Error) -> Self {
        RemoteError::Hasura(e)
    }
}

/// A remote root field's value, plus the `Set-Cookie` headers the remote sent.
#[derive(Debug, Clone)]
pub struct RemoteResponse {
    pub value: Json,
    pub set_cookies: Vec<String>,
}

/// A configured remote schema, able to talk to its server.
pub struct RemoteSchemaClient {
    pub name: Arc<str>,
    pub def: RemoteSchemaDef,
    http: reqwest::Client,
    user_agent: Arc<str>,
    env: EnvLookup,
}

impl std::fmt::Debug for RemoteSchemaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteSchemaClient({})", self.name)
    }
}

/// Client headers that are never forwarded (`forward_client_headers`).
const IGNORED_CLIENT_HEADERS: &[&str] = &[
    "content-length",
    "content-md5",
    "user-agent",
    "host",
    "origin",
    "referer",
    "accept",
    "accept-encoding",
    "accept-language",
    "accept-datetime",
    "cache-control",
    "connection",
    "dnt",
    "content-type",
];

fn remote_error(message: impl Into<String>) -> Error {
    Error::new(Code::RemoteSchemaError, message)
}

impl RemoteSchemaClient {
    pub fn new(
        name: &str,
        def: RemoteSchemaDef,
        http: reqwest::Client,
        user_agent: &str,
        env: EnvLookup,
    ) -> Self {
        RemoteSchemaClient {
            name: name.into(),
            def,
            http,
            user_agent: user_agent.into(),
            env,
        }
    }

    /// How V2 names the remote in HTTP error messages: the URL template or the env var,
    /// never the resolved URL.
    fn env_record_name(&self) -> &str {
        self.def
            .url
            .as_deref()
            .or(self.def.url_from_env.as_deref())
            .unwrap_or_default()
    }

    /// The remote's URL, resolved from the environment.
    pub fn url(&self) -> Result<String, Error> {
        if let Some(var) = &self.def.url_from_env {
            let url = (self.env)(var).ok_or_else(|| {
                Error::new(
                    Code::InvalidParams,
                    format!("environment variable '{var}' not set"),
                )
            })?;
            return reqwest::Url::parse(&url).map(|_| url).map_err(|_| {
                Error::new(
                    Code::InvalidParams,
                    format!("not a valid URI in environment variable: {var}"),
                )
            });
        }
        let template = self.def.url.as_deref().unwrap_or_default();
        let url = resolve_template(template, &*self.env).map_err(|missing| {
            Error::unexpected(format!(
                "Value for environment variables not found: {}",
                missing.join(", ")
            ))
        })?;
        reqwest::Url::parse(&url).map(|_| url).map_err(|_| {
            Error::new(
                Code::InvalidParams,
                format!("not a valid URI generated from the template: {template}"),
            )
        })
    }

    fn configured_headers(&self, confs: &[HeaderConf]) -> Result<HeaderMap, Error> {
        let mut out = HeaderMap::new();
        for h in confs {
            let value = match (&h.value, &h.value_from_env) {
                (_, Some(var)) => (self.env)(var).ok_or_else(|| {
                    Error::new(
                        Code::NotFound,
                        format!("environment variable '{var}' not set"),
                    )
                })?,
                (Some(template), None) => {
                    resolve_template(template, &*self.env).map_err(|missing| {
                        Error::new(
                            Code::NotFound,
                            format!(
                                "template cannot be resolved: Value for environment variables not found: {}",
                                missing.join(", ")
                            ),
                        )
                    })?
                }
                (None, None) => String::new(),
            };
            if let (Ok(name), Ok(value)) = (
                HeaderName::try_from(h.name.as_str()),
                HeaderValue::try_from(value),
            ) {
                out.insert(name, value);
            }
        }
        Ok(out)
    }

    fn add_default_headers(&self, headers: &mut HeaderMap) {
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Ok(ua) = HeaderValue::try_from(&*self.user_agent) {
            headers.insert(http::header::USER_AGENT, ua);
        }
    }

    fn http_error(&self, e: &reqwest::Error) -> Error {
        let message = if e.is_timeout() {
            "Response timeout".to_owned()
        } else {
            e.to_string()
        };
        let mut internal = json!({"type": "http_exception", "message": message});
        if let Some(status) = e.status() {
            internal["status"] = json!(status.as_u16());
        }
        remote_error(format!(
            "HTTP exception occurred while sending the request to {}",
            Json::String(self.env_record_name().to_owned())
        ))
        .with_internal(internal)
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: String,
    ) -> Result<(HeaderMap, Vec<u8>), Error> {
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .timeout(Duration::from_secs(self.def.timeout_seconds()))
            .body(body)
            .send()
            .await
            .map_err(|e| self.http_error(&e))?;
        let headers = resp.headers().clone();
        // The status is ignored: only the body matters (spec 08 §3.2, §6.4).
        let bytes = resp.bytes().await.map_err(|e| self.http_error(&e))?;
        Ok((headers, bytes.to_vec()))
    }

    /// Fetch the remote's introspection. Returns the raw body (for
    /// `introspect_remote_schema`) and its JSON.
    pub async fn introspect(&self) -> Result<(String, Json), Error> {
        let url = self.url()?;
        let confs = self
            .def
            .introspection_headers
            .as_deref()
            .unwrap_or(&self.def.headers);
        let mut headers = self.configured_headers(confs)?;
        self.add_default_headers(&mut headers);
        let (_, body) = self
            .post(&url, headers, introspection_request_body())
            .await?;
        let raw = String::from_utf8_lossy(&body).into_owned();
        let json: Json =
            serde_json::from_str(&raw).map_err(|e| remote_error(format!("Error in $: {e}")))?;
        Ok((raw, json))
    }

    /// Headers for a runtime request: configured headers win over session variables, which
    /// win over forwarded client headers (spec 08 §6.4).
    fn request_headers(&self, ctx: &RequestContext<'_>) -> Result<HeaderMap, Error> {
        let mut headers = HeaderMap::new();
        if self.def.forward_client_headers {
            for (name, value) in ctx.client_headers {
                let n = name.as_str();
                if IGNORED_CLIENT_HEADERS.contains(&n) || n.starts_with("x-hasura-") {
                    continue;
                }
                headers.append(name.clone(), value.clone());
            }
            for (from, to) in [
                ("host", "x-forwarded-host"),
                ("user-agent", "x-forwarded-user-agent"),
                ("origin", "x-forwarded-origin"),
            ] {
                if let Some(v) = ctx.client_headers.get(from) {
                    headers.insert(HeaderName::from_static(to), v.clone());
                }
            }
        }
        let role = HeaderValue::try_from(ctx.session.role.as_str());
        if let Ok(role) = role {
            headers.insert(HeaderName::from_static("x-hasura-role"), role);
        }
        for (name, value) in ctx.session.variables.iter() {
            let name = name.to_ascii_lowercase();
            if name == "x-hasura-admin-secret" || name == "x-hasura-role" {
                continue;
            }
            if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                headers.insert(n, v);
            }
        }
        for (name, value) in &self.configured_headers(&self.def.headers)? {
            headers.insert(name.clone(), value.clone());
        }
        self.add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Forward one remote root field and extract its value (spec 08 §6.5).
    pub async fn call(
        &self,
        call: &RemoteCall,
        ctx: &RequestContext<'_>,
        settings: RemoteSettings,
    ) -> Result<RemoteResponse, RemoteError> {
        let url = self.url()?;
        let headers = self.request_headers(ctx)?;
        let (resp_headers, body) = self.post(&url, headers, request_body(call)).await?;
        let set_cookies = resp_headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        let json: Json =
            serde_json::from_slice(&body).map_err(|e| remote_error(format!("Error in $: {e}")))?;
        let mut data = extract_data(json, settings)?;
        let mut value = data
            .remove(&call.response_key)
            .ok_or_else(|| remote_error(format!("expecting key {}", call.response_key)))?;
        if let Some(t) = &call.typenames {
            t.tree.apply(&mut value, &t.map);
        }
        Ok(RemoteResponse { value, set_cookies })
    }
}

impl RemoteSchemaClient {
    /// Send the request of a remote join and return the remote's `data` (spec 08 §9.4).
    /// Unlike root fields, any remote error fails the join, whatever the data.
    pub async fn call_join(
        &self,
        call: &RemoteCall,
        ctx: &RequestContext<'_>,
    ) -> Result<Map<String, Json>, Error> {
        let url = self.url()?;
        let headers = self.request_headers(ctx)?;
        let (_, body) = self.post(&url, headers, request_body(call)).await?;
        let json: Json = serde_json::from_slice(&body).map_err(|e| {
            Error::unexpected(format!("Remote server response is not valid JSON: {e}"))
                .with_status(500)
        })?;
        let Json::Object(mut obj) = json else {
            return Err(
                Error::unexpected("\"data\" field not found in remote response").with_status(500),
            );
        };
        if let Some(errors) = obj.remove("errors").filter(|e| !e.is_null()) {
            return Err(Error::unexpected("Errors from remote server")
                .with_internal(json!({ "errors": errors })));
        }
        match obj.remove("data") {
            Some(Json::Object(data)) => Ok(data),
            _ => Err(
                Error::unexpected("\"data\" field not found in remote response").with_status(500),
            ),
        }
    }
}

/// `{"operationName"?, "query", "variables"?}`: absent keys are omitted, never `null`.
pub fn request_body(call: &RemoteCall) -> String {
    let mut body = Map::new();
    if let Some(name) = &call.operation_name {
        body.insert("operationName".into(), Json::String(name.clone()));
    }
    body.insert("query".into(), Json::String(call.query.clone()));
    if let Some(vars) = &call.variables {
        body.insert("variables".into(), Json::Object(vars.clone()));
    }
    Json::Object(body).to_string()
}

/// The `data` object of a remote response, or its errors.
fn extract_data(json: Json, settings: RemoteSettings) -> Result<Map<String, Json>, RemoteError> {
    let Json::Object(mut obj) = json else {
        return Err(remote_error("Error in $: expected an object").into());
    };
    let errors = match obj.remove("errors") {
        None | Some(Json::Null) => None,
        Some(Json::Array(e)) if e.is_empty() => {
            return Err(remote_error("Empty \"errors\" field in response from remote").into());
        }
        Some(Json::Array(e)) => Some(e),
        Some(_) => {
            return Err(remote_error("Invalid \"errors\" field in response from remote").into());
        }
    };
    let data = obj.remove("data");
    match (data, errors) {
        (None, None) => Err(remote_error(
            "Missing \"data\" field with no errors in response from remote",
        )
        .into()),
        (Some(Json::Null), None) => Err(remote_error(
            "Received null \"data\" field with no errors in response from remote",
        )
        .into()),
        (None | Some(Json::Null), Some(errors)) => Err(RemoteError::Graphql(errors)),
        (Some(Json::Object(data)), None) => Ok(data),
        (Some(Json::Object(data)), Some(_)) if settings.prioritize_data => Ok(data),
        (Some(_), Some(errors)) => Err(RemoteError::Graphql(errors)),
        (Some(_), None) => Err(remote_error("Error in $.data: expected an object").into()),
    }
}

#[cfg(test)]
mod tests;
