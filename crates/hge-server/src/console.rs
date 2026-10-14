//! The server-hosted console (spec 11 §3.6): `GET /` redirects to `console`, `GET /console/*`
//! renders V2's console page, and `GET /console/assets/*` serves `HASURA_GRAPHQL_CONSOLE_ASSETS_DIR`.
//!
//! `console.html` is V2's `server/src-rsr/console.html` (v2.51.0, Apache-2.0), unchanged, so the
//! page loads the same console build from the same CDN path (or from the assets directory).

use std::path::{Component, Path};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hge_core::{Code, Error};

use crate::routes::{VERSION, json_response};
use crate::state::Engine;

const TEMPLATE: &str = include_str!("console.html");

/// Register the console routes. The page needs the console switch and the `metadata` API, as
/// in V2; the assets route only needs an assets directory.
pub fn register(mut router: Router<Arc<Engine>>, engine: &Engine) -> Router<Arc<Engine>> {
    let c = &engine.config;
    if c.enable_console && c.api_enabled("metadata") {
        router = router
            .route("/", get(redirect))
            .route("/console", get(page))
            .route("/console/", get(page))
            .route("/console/{*path}", get(page));
    }
    if c.console_assets_dir.is_some() {
        router = router.route("/console/assets/{*path}", get(asset));
    }
    router
}

/// V2 answers `/` with a relative `Location: console`.
async fn redirect() -> Response {
    (StatusCode::FOUND, [(header::LOCATION, "console")]).into_response()
}

async fn page(State(engine): State<Arc<Engine>>, uri: Uri) -> Response {
    // V2 renders the path its router matched: percent-decoded, without the query.
    let rest = uri.path().strip_prefix("/console").unwrap_or_default();
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let rest = percent_decode(rest);
    let mut resp = render(&engine, &rest).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

fn render(engine: &Engine, path: &str) -> String {
    let c = &engine.config;
    let no_auth = c.admin_secrets.is_empty()
        && c.jwt_secret.is_none()
        && c.jwt_secrets.is_none()
        && c.auth_hook.is_none();
    let console_path = if path.is_empty() {
        "/console".to_owned()
    } else {
        format!("/console/{path}")
    };
    let vars: [(&str, String); 9] = [
        ("consolePath", console_path),
        ("isAdminSecretSet", (!no_auth).to_string()),
        ("isAdminSecretDisabled", c.disable_admin_secret.to_string()),
        ("enableTelemetry", c.enable_telemetry.to_string()),
        ("assetsVersion", format!("channel/versioned/{VERSION}")),
        ("cdnAssets", c.console_assets_dir.is_none().to_string()),
        ("serverVersion", VERSION.to_owned()),
        ("consoleType", "oss".to_owned()),
        // V2 sets the key twice and the empty value wins.
        ("consoleSentryDsn", String::new()),
    ];
    let mut html = TEMPLATE.to_owned();
    for (name, value) in vars {
        html = html.replace(&format!("{{{{{name}}}}}"), &escape_html(&value));
    }
    html
}

/// Mustache's `{{…}}` escaping, which V2's template renderer applies.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn asset(State(engine): State<Arc<Engine>>, uri: Uri) -> Response {
    let Some(dir) = engine.config.console_assets_dir.clone() else {
        return not_found("");
    };
    let path = percent_decode(
        uri.path()
            .strip_prefix("/console/assets/")
            .unwrap_or_default(),
    );
    let relative = Path::new(&path);
    let valid = !path.contains("..")
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !valid {
        return not_found(&path);
    }
    let file = Path::new(&dir).join(relative);
    let read = {
        let file = file.clone();
        tokio::task::spawn_blocking(move || std::fs::read(file)).await
    };
    match read {
        Ok(Ok(bytes)) => {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let (name, gzip) = match name.strip_suffix(".gz") {
                Some(inner) => (inner, true),
                None => (name, false),
            };
            let mut resp = bytes.into_response();
            let headers = resp.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime(name)));
            if gzip {
                headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            }
            resp
        }
        _ => not_found(&path),
    }
}

fn not_found(path: &str) -> Response {
    let err = Error::new(
        Code::NotFound,
        format!("Couldn't find console asset {path}"),
    );
    json_response(
        StatusCode::NOT_FOUND,
        err.to_api_json(false).to_string(),
        None,
    )
}

/// The types Haskell's `defaultMimeLookup` gives the files a console build contains.
fn mime(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("js" | "mjs") => "application/javascript",
        Some("css") => "text/css",
        Some("html" | "htm") => "text/html",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_mustache_and_decodes_paths() {
        assert_eq!(
            escape_html(&percent_decode("a%22b%3Cc'&>")),
            "a&quot;b&lt;c&#39;&amp;&gt;"
        );
        assert_eq!(percent_decode("data/%zz/x"), "data/%zz/x");
    }

    #[test]
    fn template_has_every_variable_v2_fills() {
        for name in [
            "consolePath",
            "isAdminSecretSet",
            "isAdminSecretDisabled",
            "enableTelemetry",
            "assetsVersion",
            "cdnAssets",
            "serverVersion",
            "consoleType",
            "consoleSentryDsn",
        ] {
            assert!(TEMPLATE.contains(&format!("{{{{{name}}}}}")), "{name}");
        }
    }

    #[test]
    fn gzipped_assets_take_the_inner_type() {
        assert_eq!(mime("main.js"), "application/javascript");
        assert_eq!(mime("x.js.map"), "application/octet-stream");
        assert_eq!(mime("font.woff2"), "font/woff2");
    }
}
