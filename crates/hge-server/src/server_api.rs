//! `POST /v1alpha1/pg_dump` (spec 10 §7.2), which `hasura migrate create --from-server` and
//! `hasura seed create --from-table` call. `/v1/version` and `/v1alpha1/config` live in
//! `telemetry::endpoints`.

use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hge_core::{Code, Error};
use serde_json::Value;

use crate::routes::{json_response, request_id};
use crate::state::Engine;

/// `POST /v1alpha1/pg_dump` (admin only): run `pg_dump` against a Postgres source, as
/// `hasura migrate create --from-server` and `hasura seed create --from-table` do.
pub async fn pg_dump(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id(&headers);
    if let Err(resp) = crate::telemetry::endpoints::only_admin(&engine, &headers).await {
        return resp;
    }
    let internal = engine.config.admin_internal_errors;
    let error = |status: StatusCode, e: Error| {
        json_response(
            status,
            e.to_api_json(internal).to_string(),
            Some(rid.clone()),
        )
    };
    let request = match PgDumpRequest::parse(&body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let state = engine.state.load();
    let url = state
        .metadata
        .sources
        .iter()
        .find(|s| s.name == request.source && matches!(s.kind.as_str(), "postgres" | "pg"))
        .and_then(|s| crate::load::source_url(s, &engine.config).ok());
    let Some(url) = url else {
        return error(
            StatusCode::BAD_REQUEST,
            Error::new(
                Code::NotFound,
                format!("source \"{}\" not found", request.source),
            )
            .with_path("$"),
        );
    };
    let output = tokio::task::spawn_blocking(move || {
        let program = pg_dump_program();
        let mut args = vec![url, "--encoding=utf8".to_owned()];
        if supports_restrict_key(&program) {
            args.push("--restrict-key=hasura".to_owned());
        }
        args.extend(request.opts);
        std::process::Command::new(&program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
    })
    .await;
    match output {
        Ok(Ok(out)) if out.status.success() => {
            let sql = String::from_utf8_lossy(&out.stdout);
            let sql = if request.clean_output {
                clean_pg_dump(&sql)
            } else {
                sql.into_owned()
            };
            let mut resp = (StatusCode::OK, sql).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/sql; charset=utf-8"),
            );
            if let Ok(id) = HeaderValue::from_str(&rid) {
                resp.headers_mut().insert("x-request-id", id);
            }
            resp
        }
        Ok(Ok(out)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Error::new(Code::Unexpected, "error while executing pg_dump")
                .with_path("$")
                .with_internal(Value::String(
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )),
        ),
        Ok(Err(e)) => {
            tracing::warn!(target: "http-log", error = %e, "cannot run pg_dump");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Error::new(
                    Code::Unexpected,
                    "internal exception while executing pg_dump",
                )
                .with_path("$"),
            )
        }
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Error::new(Code::Unexpected, format!("pg_dump task failed: {e}")).with_path("$"),
        ),
    }
}

struct PgDumpRequest {
    opts: Vec<String>,
    clean_output: bool,
    source: String,
}

impl PgDumpRequest {
    fn parse(body: &[u8]) -> Result<Self, Error> {
        let failed = |m: String| Error::new(Code::ParseFailed, m).with_path("$");
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| Error::new(Code::InvalidJson, e.to_string()).with_path("$"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| failed("expected Object, but encountered something else".into()))?;
        let opts = match obj.get("opts") {
            None => return Err(failed("key \"opts\" not found".into())),
            Some(Value::Array(items)) => items
                .iter()
                .map(|o| o.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| failed("expected String".into()))?,
            Some(_) => return Err(failed("expected Array".into())),
        };
        let clean_output = match obj.get("clean_output") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(failed("expected Boolean".into())),
        };
        let source = match obj.get("source") {
            None | Some(Value::Null) => "default".to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(failed("expected String".into())),
        };
        Ok(PgDumpRequest {
            opts,
            clean_output,
            source,
        })
    }
}

/// `HASURA_GRAPHQL_PG_DUMP_PATH` (our addition), else `pg_dump` on `PATH`.
fn pg_dump_program() -> String {
    std::env::var("HASURA_GRAPHQL_PG_DUMP_PATH").unwrap_or_else(|_| "pg_dump".to_owned())
}

/// `--restrict-key` (pg_dump 17.6 / 16.10 and later), which V2 always passes. Older
/// `pg_dump`s reject it, so it is only passed when `--help` lists it.
fn supports_restrict_key(program: &str) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        std::process::Command::new(program)
            .arg("--help")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--restrict-key"))
    })
}

/// V2's `clean_output`: drop comments, blank lines, the session `SET`s, the `public` schema
/// boilerplate and Hasura's own event triggers (`PGDump.hs`).
fn clean_pg_dump(sql: &str) -> String {
    const DROP: &[&str] = &[
        "SET statement_timeout = 0;",
        "SET lock_timeout = 0;",
        "SET idle_in_transaction_session_timeout = 0;",
        "SET client_encoding = 'UTF8';",
        "SET standard_conforming_strings = on;",
        "SELECT pg_catalog.set_config('search_path', '', false);",
        "SET xmloption = content;",
        "SET client_min_messages = warning;",
        "SET row_security = off;",
        "SET default_tablespace = '';",
        "SET default_with_oids = false;",
        "SET default_table_access_method = heap;",
        "CREATE SCHEMA public;",
        "COMMENT ON SCHEMA public IS 'standard public schema';",
    ];
    let mut out = String::with_capacity(sql.len());
    for line in sql.lines() {
        if line.trim().is_empty()
            || line.starts_with("--")
            || DROP.contains(&line)
            || is_hasura_trigger(line)
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `^CREATE TRIGGER "?notify_hasura_.+"? AFTER [[:alnum:]]+ ON .+ FOR EACH ROW EXECUTE
/// (FUNCTION|PROCEDURE) "?hdb_catalog"?\."?notify_hasura_.+"?\(\);$`
fn is_hasura_trigger(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("CREATE TRIGGER ") else {
        return false;
    };
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    if !rest.starts_with("notify_hasura_") {
        return false;
    }
    let Some((head, func)) = rest
        .split_once(" FOR EACH ROW EXECUTE FUNCTION ")
        .or_else(|| rest.split_once(" FOR EACH ROW EXECUTE PROCEDURE "))
    else {
        return false;
    };
    let after_ok = head.split_once(" AFTER ").is_some_and(|(_, ev)| {
        ev.split_once(" ON ").is_some_and(|(event, table)| {
            !event.is_empty() && event.chars().all(char::is_alphanumeric) && !table.is_empty()
        })
    });
    let func = func
        .strip_prefix("\"hdb_catalog\"")
        .or_else(|| func.strip_prefix("hdb_catalog"));
    let func_ok = func.and_then(|f| f.strip_prefix('.')).is_some_and(|f| {
        let f = f.strip_prefix('"').unwrap_or(f);
        f.starts_with("notify_hasura_") && f.ends_with("();")
    });
    after_ok && func_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_output_drops_what_v2_drops() {
        let dump = "--\n-- PostgreSQL database dump\n--\n\n\\restrict hasura\n\
            SET statement_timeout = 0;\nSET transaction_timeout = 0;\n\
            SELECT pg_catalog.set_config('search_path', '', false);\n\
            CREATE TABLE public.a (\n    id integer NOT NULL\n);\n\n\
            CREATE TRIGGER notify_hasura_t_INSERT AFTER INSERT ON public.a FOR EACH ROW EXECUTE FUNCTION hdb_catalog.\"notify_hasura_t_INSERT\"();\n\
            CREATE TRIGGER mine AFTER INSERT ON public.a FOR EACH ROW EXECUTE FUNCTION public.f();\n";
        assert_eq!(
            clean_pg_dump(dump),
            "\\restrict hasura\nSET transaction_timeout = 0;\nCREATE TABLE public.a (\n    id integer NOT NULL\n);\n\
             CREATE TRIGGER mine AFTER INSERT ON public.a FOR EACH ROW EXECUTE FUNCTION public.f();\n"
        );
    }

    #[test]
    fn pg_dump_request_defaults_and_errors() {
        let r = PgDumpRequest::parse(br#"{"opts": ["-t", "a"]}"#).unwrap();
        assert_eq!(
            (r.opts.len(), r.clean_output, r.source.as_str()),
            (2, false, "default")
        );
        let e = PgDumpRequest::parse(b"{}").err().unwrap();
        assert_eq!(
            e.to_api_json(false).to_string(),
            r#"{"error":"key \"opts\" not found","path":"$","code":"parse-failed"}"#
        );
    }
}
