//! HTTP access to the engine: `/v1/metadata`, `/v2/query`, `/v1/version`, `pg_dump`.

use std::time::Duration;

use reqwest::blocking::Client as Http;
use serde_json::Value;

use crate::config::Config;
use crate::error::{Error, Result};

pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn ok(&self) -> bool {
        self.status == 200
    }

    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.body).map_err(|e| Error(e.to_string()))
    }
}

pub struct Client {
    http: Http,
    admin_secret: Option<String>,
    pub config: Config,
}

impl Client {
    pub fn new(config: &Config) -> Result<Client> {
        let mut builder = Http::builder().timeout(None::<Duration>);
        if config.insecure_skip_tls_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if !config.certificate_authority.is_empty() {
            let pem = std::fs::read(&config.certificate_authority).map_err(|e| {
                Error(format!(
                    "error while getting TLS config: error reading CA {}: {e}",
                    config.certificate_authority
                ))
            })?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .map_err(|_| Error("error while getting TLS config".into()))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().map_err(|e| {
            Error(format!(
                "error while creating http client with TLS configuration {e}"
            ))
        })?;
        Ok(Client {
            http,
            admin_secret: None,
            config: config.clone(),
        })
    }

    /// Send the admin secret on later requests (after the server check).
    pub fn set_admin_secret(&mut self, secret: &str) {
        self.admin_secret = Some(secret.to_string());
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        let mut req = self.http.get(url);
        if let Some(s) = &self.admin_secret {
            req = req.header("X-Hasura-Admin-Secret", s);
        }
        let resp = req
            .send()
            .map_err(|e| Error(network_error("Get", url, &e)))?;
        Ok(read_response(resp))
    }

    pub fn post(&self, url: &str, body: &Value) -> Result<Response> {
        let mut req = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(body).unwrap_or_default());
        if let Some(s) = &self.admin_secret {
            req = req.header("X-Hasura-Admin-Secret", s);
        }
        let resp = req
            .send()
            .map_err(|e| Error(network_error("Post", url, &e)))?;
        Ok(read_response(resp))
    }

    /// POST an already-encoded JSON body.
    pub fn post_raw(&self, url: &str, body: String) -> Result<Response> {
        let mut req = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(body);
        if let Some(s) = &self.admin_secret {
            req = req.header("X-Hasura-Admin-Secret", s);
        }
        let resp = req
            .send()
            .map_err(|e| Error(network_error("Post", url, &e)))?;
        Ok(read_response(resp))
    }

    /// POST a `{type, args}` request to `/v1/metadata`.
    pub fn metadata(&self, body: &Value) -> Result<Response> {
        self.post(&self.config.v1_metadata_endpoint(), body)
    }

    /// A metadata call that must succeed; on any other status the error is the body.
    pub fn metadata_ok(&self, body: &Value) -> Result<String> {
        let r = self.metadata(body)?;
        if !r.ok() {
            return Err(Error(r.body));
        }
        Ok(r.body)
    }

    pub fn v2_query(&self, body: &Value) -> Result<Response> {
        self.post(&self.config.v2_query_endpoint(), body)
    }

    /// `export_metadata` (v1): the server's metadata document.
    pub fn export_metadata(&self) -> Result<Value> {
        let body = self.metadata_ok(&serde_json::json!({"type": "export_metadata", "args": {}}))?;
        serde_json::from_str(&body).map_err(|e| Error(e.to_string()))
    }

    /// A `run_sql` through `/v2/query`; a failure's message is the response body.
    pub fn run_sql(&self, args: Value) -> Result<Value> {
        let r = self.v2_query(&serde_json::json!({"type": "run_sql", "args": args}))?;
        if !r.ok() {
            if r.body.is_empty() {
                return Err(Error(format!("run_sql api request failed {}", r.status)));
            }
            return Err(Error(r.body));
        }
        r.json()
    }

    /// `pg_dump` through the engine.
    pub fn pg_dump(&self, opts: &[String], source: &str) -> Result<String> {
        let mut body = serde_json::json!({"opts": opts, "clean_output": true});
        if !source.is_empty() {
            body["source"] = Value::String(source.to_string());
        }
        let r = self.post(&self.config.pg_dump_endpoint(), &body)?;
        if !r.ok() {
            return Err(Error(format!("pg_dump request: {} \n{}", r.status, r.body)));
        }
        Ok(r.body)
    }
}

/// The CLI's HTTP client re-indents JSON bodies (`json.Indent` with two spaces); error
/// messages that quote a body show it that way.
fn read_response(resp: reqwest::blocking::Response) -> Response {
    let status = resp.status().as_u16();
    let is_json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("application/json"));
    let body = resp.text().unwrap_or_default();
    let body = if is_json {
        crate::gojson::indent(&body, "  ").unwrap_or(body)
    } else {
        body
    };
    Response { status, body }
}

fn network_error(method: &str, url: &str, e: &reqwest::Error) -> String {
    let mut cause = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        cause = s.to_string();
        source = s.source();
    }
    format!("{method} {}: {cause}", crate::gofmt::quote(url))
}
