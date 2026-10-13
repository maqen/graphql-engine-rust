//! The outbound HTTP seam: a small trait so delivery logic can be tested with fakes, and a
//! `reqwest` implementation.

use std::time::Duration;

use futures::future::BoxFuture;

#[derive(Debug, Clone)]
pub struct WebhookRequest {
    pub url: String,
    /// Sent in order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct WebhookResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl WebhookResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Why no response was received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookError {
    /// The URL could not be parsed.
    InvalidUrl(String),
    /// Connection failure, timeout, protocol error …
    Http { message: String, timeout: bool },
}

pub trait WebhookClient: Send + Sync {
    fn post(&self, req: WebhookRequest) -> BoxFuture<'_, Result<WebhookResponse, WebhookError>>;
}

/// Pooled HTTP/1.1 + HTTP/2 client with rustls.
pub struct HttpClient {
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(HttpClient { client })
    }
}

impl WebhookClient for HttpClient {
    fn post(&self, req: WebhookRequest) -> BoxFuture<'_, Result<WebhookResponse, WebhookError>> {
        Box::pin(async move {
            let url = reqwest::Url::parse(&req.url)
                .map_err(|e| WebhookError::InvalidUrl(e.to_string()))?;
            let mut builder = self.client.post(url).timeout(req.timeout).body(req.body);
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            let resp = builder.send().await.map_err(http_error)?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_owned(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = resp.bytes().await.map_err(http_error)?.to_vec();
            Ok(WebhookResponse {
                status,
                headers,
                body,
            })
        })
    }
}

fn http_error(e: reqwest::Error) -> WebhookError {
    let timeout = e.is_timeout();
    let message = if timeout {
        "Response timeout".to_owned()
    } else if e.is_connect() {
        let mut msg = String::from("Connection failure");
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            msg = format!("Connection failure: {s}");
            source = s.source();
        }
        msg
    } else {
        e.to_string()
    };
    WebhookError::Http { message, timeout }
}
