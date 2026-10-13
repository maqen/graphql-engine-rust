//! V2's `HttpException` JSON (`Hasura.HTTP.getHttpExceptionJson` with
//! `ShowErrorInfo False`), as logged in `webhook-log`'s `http_error` and in
//! `jwk-refresh-log`'s `http_exception`.

use serde_json::{Value, json};

/// `{"type":"http_exception","message":…,"request":{…}}` for a failed outgoing request.
/// The request object has V2's keys only; headers are never included (V2 dropped them so
/// no secret can leak).
pub(crate) fn to_json(err: &reqwest::Error, method: &str, url: &str) -> Value {
    json!({
        "type": "http_exception",
        "message": message(err),
        "request": request_json(method, url),
    })
}

/// V2's `serializeHTTPExceptionMessage`: a fixed text per http-client exception. reqwest
/// reports fewer kinds; anything we cannot tell apart is V2's catch-all `"unexpected"`.
fn message(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        if err.is_connect() {
            "Connection timeout"
        } else {
            "Response timeout"
        }
    } else if err.is_connect() {
        "Connection failure"
    } else if err.is_redirect() {
        "Too many redirects"
    } else {
        "unexpected"
    }
}

/// V2's `encodeHTTPRequestJSON`. http-client defaults the path to `/` and keeps the `?` of
/// the query string; the per-request timeout is never set, so V2 always shows
/// `ResponseTimeoutDefault` (the `Show` of http-client's `ResponseTimeout`).
fn request_json(method: &str, url: &str) -> Value {
    let parsed = reqwest::Url::parse(url).ok();
    let secure = parsed.as_ref().is_some_and(|u| u.scheme() == "https");
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or_default();
    let port = parsed
        .as_ref()
        .and_then(|u| u.port_or_known_default())
        .unwrap_or(if secure { 443 } else { 80 });
    let path = match parsed.as_ref().map(|u| u.path()) {
        Some(p) if !p.is_empty() => p,
        _ => "/",
    };
    let query = parsed
        .as_ref()
        .and_then(|u| u.query())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    json!({
        "host": host,
        "port": port,
        "secure": secure,
        "path": path,
        "queryString": query,
        "method": method,
        "responseTimeout": "ResponseTimeoutDefault",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request() {
        assert_eq!(
            request_json("POST", "https://user:pw@auth.example.com/hook?a=1"),
            json!({
                "host": "auth.example.com",
                "port": 443,
                "secure": true,
                "path": "/hook",
                "queryString": "?a=1",
                "method": "POST",
                "responseTimeout": "ResponseTimeoutDefault",
            })
        );
        let r = request_json("GET", "http://127.0.0.1:8080");
        assert_eq!(r["port"], 8080);
        assert_eq!(r["path"], "/");
        assert_eq!(r["queryString"], "");
        assert_eq!(r["secure"], false);
    }
}
