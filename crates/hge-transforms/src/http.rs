//! A transport-independent HTTP request (the parts of http-client's `Request` that
//! transforms read and write), with V2's URL and query-string rules.

use serde_json::{Value, json};

/// A request about to be sent to a webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// `https` when true, `http` otherwise.
    pub secure: bool,
    /// Host as written in the URL (with any `user:pass@` prefix, like V2's `setUrl`).
    pub host: String,
    pub port: u16,
    /// Path as written (percent-encoded); may be empty after a URL transform.
    pub path: String,
    /// Decoded query parameters; `None` is a key without `=`.
    pub query: Vec<(String, Option<String>)>,
    /// Headers in send order; names keep their case and may repeat.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The parts of an absolute URI (RFC 3986, as `Network.URI.parseURI` reads it).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Uri<'a> {
    scheme: &'a str,
    /// `(userinfo, host, port)` when the URI has an authority (`//…`).
    authority: Option<(Option<&'a str>, &'a str, &'a str)>,
    path: &'a str,
    /// Without the leading `?`.
    query: Option<&'a str>,
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn is_reserved(b: u8) -> bool {
    matches!(
        b,
        b':' | b'/'
            | b'?'
            | b'#'
            | b'['
            | b']'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

/// `true` if every character may appear in a URI and every `%` starts an escape.
fn valid_uri_chars(s: &str, allow_brackets: bool) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len()
                    || !b[i + 1].is_ascii_hexdigit()
                    || !b[i + 2].is_ascii_hexdigit()
                {
                    return false;
                }
                i += 3;
                continue;
            }
            b'[' | b']' if !allow_brackets => return false,
            c if is_unreserved(c) || is_reserved(c) => {}
            _ => return false,
        }
        i += 1;
    }
    true
}

fn parse_uri(s: &str) -> Option<Uri<'_>> {
    let colon = s.find(':')?;
    let scheme = &s[..colon];
    let mut chars = scheme.bytes();
    if !chars.next()?.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
    {
        return None;
    }
    let rest = &s[colon + 1..];
    let (rest, fragment) = match rest.find('#') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (rest, query) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (authority, path) = match rest.strip_prefix("//") {
        Some(r) => {
            let end = r.find('/').unwrap_or(r.len());
            (Some(&r[..end]), &r[end..])
        }
        None => (None, rest),
    };
    if !valid_uri_chars(path, false)
        || !query.is_none_or(|q| valid_uri_chars(q, false))
        || !fragment.is_none_or(|f| valid_uri_chars(f, false))
    {
        return None;
    }
    let authority = match authority {
        None => None,
        Some(a) => {
            let (userinfo, hostport) = match a.rfind('@') {
                Some(i) => (Some(&a[..i]), &a[i + 1..]),
                None => (None, a),
            };
            if userinfo.is_some_and(|u| u.contains(['[', ']', '/']) || !valid_uri_chars(u, false)) {
                return None;
            }
            let (host, port) = if hostport.starts_with('[') {
                let end = hostport.find(']')? + 1;
                let port = &hostport[end..];
                (&hostport[..end], port.strip_prefix(':').unwrap_or(port))
            } else {
                match hostport.rfind(':') {
                    Some(i) => (&hostport[..i], &hostport[i + 1..]),
                    None => (hostport, ""),
                }
            };
            if !port.bytes().all(|c| c.is_ascii_digit())
                || !valid_uri_chars(host, true)
                || (!host.starts_with('[') && host.contains(['[', ']']))
            {
                return None;
            }
            Some((userinfo, host, port))
        }
    };
    Some(Uri {
        scheme,
        authority,
        path,
        query,
    })
}

/// `true` if `s` is an absolute URI (`Network.URI.parseURI` succeeds).
pub fn is_absolute_uri(s: &str) -> bool {
    parse_uri(s).is_some()
}

fn default_port(secure: bool) -> u16 {
    if secure { 443 } else { 80 }
}

/// Percent-encodes `s`, keeping bytes for which `keep` holds.
fn percent_encode(s: &str, keep: impl Fn(u8) -> bool, space_plus: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if keep(b) {
            out.push(b as char);
        } else if space_plus && b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// http-types' `urlEncode True` (query strings): unreserved = `A-Za-z0-9-_.~`, space → `+`.
pub fn encode_query_component(s: &str) -> String {
    percent_encode(s, is_unreserved, true)
}

/// network-uri's `escapeURIString isUnescapedInURIComponent` (form bodies): unreserved =
/// `A-Za-z0-9-_.!~*'()`.
pub fn encode_uri_component(s: &str) -> String {
    percent_encode(
        s,
        |b| is_unreserved(b) || matches!(b, b'!' | b'*' | b'\'' | b'(' | b')'),
        false,
    )
}

/// network-uri's `escapeURIString isAllowedInURI` (http-client's `parseRequest`).
fn escape_disallowed(s: &str) -> String {
    percent_encode(
        s,
        |b| is_unreserved(b) || is_reserved(b) || b == b'%',
        false,
    )
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// http-types' `urlDecode True`: `+` is a space, `%XX` an escaped byte.
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// http-types' `parseQuery`: an optional leading `?`, pairs split on `&` and `;`.
pub fn parse_query(s: &str) -> Vec<(String, Option<String>)> {
    let s = s.strip_prefix('?').unwrap_or(s);
    if s.is_empty() {
        return vec![];
    }
    let mut pieces: Vec<&str> = s.split(['&', ';']).collect();
    // A trailing separator does not start another pair.
    if pieces.last() == Some(&"") {
        pieces.pop();
    }
    pieces
        .into_iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (url_decode(k), Some(url_decode(v))),
            None => (url_decode(p), None),
        })
        .collect()
}

/// http-types' `renderQuery True`: `?k=v&k2`, or the empty string.
pub fn render_query(query: &[(String, Option<String>)]) -> String {
    if query.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = query
        .iter()
        .map(|(k, v)| match v {
            Some(v) => format!(
                "{}={}",
                encode_query_component(k),
                encode_query_component(v)
            ),
            None => encode_query_component(k),
        })
        .collect();
    format!("?{}", pairs.join("&"))
}

/// `unEscapeString`: `%XX` escapes decoded (`+` stays a plus).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `Authorization: Basic …` header for URL userinfo (`user:pass`), as http-client's
/// `applyBasicAuth` builds it.
fn basic_auth(userinfo: &str) -> (String, String) {
    use base64::Engine;
    let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let credentials = format!("{}:{}", unescape(user), unescape(pass));
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
    ("Authorization".to_owned(), format!("Basic {encoded}"))
}

/// Where a URL transform result points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlTarget {
    /// An absolute URI with an `http(s)`-usable authority.
    Valid,
    /// An absolute URI without authority (`mailto:x`): V2's `setUrl` keeps the old URL.
    NoAuthority,
    /// Not an absolute URI, or an authority we refuse: userinfo (V2 would send the host
    /// `user@host`, which never resolves; following it would let a template redirect the
    /// request with all its headers) or a port that is not a number up to 65535.
    Invalid,
}

pub(crate) fn url_target(url: &str) -> UrlTarget {
    match parse_uri(url) {
        None => UrlTarget::Invalid,
        Some(Uri {
            authority: None, ..
        }) => UrlTarget::NoAuthority,
        Some(Uri {
            authority: Some((userinfo, host, port)),
            ..
        }) => {
            if userinfo.is_some()
                || host.is_empty()
                || (!port.is_empty() && port.parse::<u16>().is_err())
            {
                UrlTarget::Invalid
            } else {
                UrlTarget::Valid
            }
        }
    }
}

impl Request {
    /// http-client's `parseRequest`: characters not allowed in URIs are escaped first, the
    /// scheme must be `http` or `https`, an empty path becomes `/`, and userinfo becomes an
    /// `Authorization: Basic` header in front of `headers` (`applyAnyUriBasedAuth`). The
    /// error is the text of V2's `InvalidUrlException`.
    pub fn new(
        method: impl Into<String>,
        url: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Request, String> {
        let escaped = escape_disallowed(url);
        let invalid = |reason: &str| format!("InvalidUrlException \"{url}\" \"{reason}\"");
        let uri = parse_uri(&escaped).ok_or_else(|| invalid("Invalid URL"))?;
        let secure = match uri.scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            _ => return Err(invalid("Invalid scheme")),
        };
        let (userinfo, host, port) = uri.authority.ok_or_else(|| invalid("Invalid URL"))?;
        if host.is_empty() {
            return Err(invalid("Invalid URL"));
        }
        let port = match port {
            "" => default_port(secure),
            p => p.parse().map_err(|_| invalid("Invalid URL"))?,
        };
        let headers = match userinfo {
            Some(u) => std::iter::once(basic_auth(u)).chain(headers).collect(),
            None => headers,
        };
        Ok(Request {
            method: method.into(),
            secure,
            host: host.to_owned(),
            port,
            path: if uri.path.is_empty() {
                "/".to_owned()
            } else {
                uri.path.to_owned()
            },
            query: uri.query.map(parse_query).unwrap_or_default(),
            headers,
            body,
        })
    }

    /// V2's `setUrl`: takes host, port, TLS flag, path and query string from `url`. A URL
    /// that is not [`UrlTarget::Valid`] leaves the request unchanged (transforms reject
    /// invalid ones before getting here).
    pub fn set_url(&mut self, url: &str) {
        if url_target(url) != UrlTarget::Valid {
            return;
        }
        let Some(Uri {
            scheme,
            authority: Some((_, host, port)),
            path,
            query,
        }) = parse_uri(url)
        else {
            return;
        };
        let secure = scheme.eq_ignore_ascii_case("https");
        self.port = match port {
            "" => default_port(secure),
            p => p.parse().unwrap_or(default_port(secure)),
        };
        self.secure = secure;
        self.host = host.to_owned();
        self.path = path.to_owned();
        self.query = query.map(parse_query).unwrap_or_default();
    }

    /// `scheme://host[:port]<path>`, without the query string (V2's `getUrl`).
    pub fn url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        if self.port == default_port(self.secure) {
            format!("{scheme}://{}{}", self.host, self.path)
        } else {
            format!("{scheme}://{}:{}{}", self.host, self.port, self.path)
        }
    }

    /// `?k=v…`, or the empty string.
    pub fn query_string(&self) -> String {
        render_query(&self.query)
    }

    /// The URL with its query string, as sent.
    pub fn full_url(&self) -> String {
        self.url() + &self.query_string()
    }

    /// V2's `ToJSON Client.Request` (the `transformed_request` of action error details).
    pub fn to_json(&self, response_timeout_micros: Option<u128>) -> Value {
        self.to_json_with_body(&self.body, response_timeout_micros)
    }

    /// [`Request::to_json`] for a request whose body was moved out (into the transport). Its
    /// `url` is http-client's `getUri`, which keeps the query string.
    pub fn to_json_with_body(&self, body: &[u8], response_timeout_micros: Option<u128>) -> Value {
        let headers: Vec<Value> = self.headers.iter().map(|(k, v)| json!([k, v])).collect();
        json!({
            "url": self.full_url(),
            "method": self.method,
            "headers": headers,
            "body": std::str::from_utf8(body).ok(),
            "query_string": self.query_string(),
            "response_timeout": response_timeout_micros.map(|m| m.to_string()),
        })
    }

    /// The query parameters as `[[key, value-or-null], …]` (`$query_params`).
    pub fn query_json(&self) -> Value {
        query_json(&self.query)
    }
}

/// Query parameters as `[[key, value-or-null], …]`.
pub fn query_json(query: &[(String, Option<String>)]) -> Value {
    query.iter().map(|(k, v)| json!([k, v])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_parse_request() {
        let r = Request::new("POST", "http://localhost:1234", vec![], vec![]).unwrap();
        assert_eq!(r.full_url(), "http://localhost:1234/");
        let r = Request::new("POST", "https://h:443/a/b?x=1&y&z=a+b%2C", vec![], vec![]).unwrap();
        assert_eq!((r.secure, r.port, r.path.as_str()), (true, 443, "/a/b"));
        assert_eq!(
            r.query,
            vec![
                ("x".into(), Some("1".into())),
                ("y".into(), None),
                ("z".into(), Some("a b,".into()))
            ]
        );
        assert_eq!(r.full_url(), "https://h/a/b?x=1&y&z=a+b%2C");
        let r = Request::new("POST", "http://h/a b", vec![], vec![]).unwrap();
        assert_eq!(r.path, "/a%20b");
        assert!(Request::new("POST", "ftp://h", vec![], vec![]).is_err());
        assert!(Request::new("POST", "localhost:1234/x", vec![], vec![]).is_err());
        assert_eq!(
            Request::new("POST", "nope", vec![], vec![]).unwrap_err(),
            "InvalidUrlException \"nope\" \"Invalid URL\""
        );
    }

    #[test]
    fn set_url_keeps_empty_path_and_ignores_bad_urls() {
        let mut r = Request::new("POST", "http://localhost:1234?a=1", vec![], vec![]).unwrap();
        r.set_url("http://www.google.com");
        assert_eq!(r.full_url(), "http://www.google.com");
        r.set_url("https://example.com:8443/x?q=1");
        assert_eq!(r.full_url(), "https://example.com:8443/x?q=1");
        r.set_url("not a url");
        r.set_url("mailto:someone@example.com");
        // Userinfo must not move the request to another host; nor may an overflowing port.
        r.set_url("http://h@evil.com/");
        r.set_url("http://h:70000/");
        assert_eq!(r.full_url(), "https://example.com:8443/x?q=1");
        assert_eq!(url_target("http://h@evil.com/"), UrlTarget::Invalid);
        assert_eq!(url_target("http://h:70000/"), UrlTarget::Invalid);
        assert_eq!(url_target("mailto:x@y"), UrlTarget::NoAuthority);
        assert_eq!(url_target("http://h:65535/"), UrlTarget::Valid);
    }

    #[test]
    fn userinfo_becomes_basic_auth() {
        let headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        let r = Request::new("POST", "http://us%40r:p%3Ass@h:8080/x", headers, vec![]).unwrap();
        // base64("us@r:p:ss")
        assert_eq!(
            r.headers,
            vec![
                ("Authorization".to_owned(), "Basic dXNAcjpwOnNz".to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ]
        );
        assert_eq!(
            (r.host.as_str(), r.full_url()),
            ("h", "http://h:8080/x".to_owned())
        );
        let r = Request::new("POST", "http://user@h", vec![], vec![]).unwrap();
        assert_eq!(r.headers[0].1, "Basic dXNlcjo=");
    }

    #[test]
    fn uri_validation() {
        assert!(is_absolute_uri("http://example.com/users/1"));
        assert!(is_absolute_uri("mailto:x@y"));
        assert!(is_absolute_uri("http://[::1]:80/"));
        assert!(!is_absolute_uri("/relative"));
        assert!(!is_absolute_uri("http://example.com/a b"));
        assert!(!is_absolute_uri("http://example.com/%zz"));
        assert!(!is_absolute_uri("http://example.com/{x}"));
        assert!(!is_absolute_uri("http://h:8a/"));
    }

    #[test]
    fn query_strings() {
        assert_eq!(
            parse_query("?tags=apple,banana,;x&&y=%41"),
            vec![
                ("tags".into(), Some("apple,banana,".into())),
                ("x".into(), None),
                ("".into(), None),
                ("y".into(), Some("A".into())),
            ]
        );
        assert_eq!(
            render_query(&[("tags".into(), Some("apple,banana,".into()))]),
            "?tags=apple%2Cbanana%2C"
        );
        assert_eq!(
            render_query(&[("a b".into(), Some("é~".into()))]),
            "?a+b=%C3%A9~"
        );
        assert_eq!(encode_uri_component("a b!*'()~&="), "a%20b!*'()~%26%3D");
    }
}
