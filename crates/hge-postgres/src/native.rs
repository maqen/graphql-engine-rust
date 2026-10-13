//! Native query validation: resolve the scalar types logical models and native queries name,
//! and prepare (never execute) each native query's validation statement.
//!
//! Everything runs in one `BEGIN READ ONLY … ROLLBACK` transaction over the extended
//! protocol, so the native query code is only ever parsed as a single statement; a savepoint
//! around each step keeps one failure from aborting the others.

use std::collections::{HashMap, HashSet};

use hge_core::{ColumnType, Error, NamedType, NativeQueryCheck, ResultColumn};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::Client;
use tokio_postgres::types::{Kind, Type};

use crate::source::pg_error;

/// The outcome of each check: result columns, or V2's `error` object.
pub type CheckResults = HashMap<NativeQueryCheck, Result<Vec<ResultColumn>, Value>>;

/// Describes the given type oids (`$1`: a JSON array of oids). Domains resolve to their base
/// type and arrays to their element type, as for table columns.
const TYPES_SQL: &str = r#"
SELECT coalesce(json_agg(json_build_object(
  'oid', ty.oid::int8,
  'schema', coalesce(et.ns, bt.ns, tn.nspname),
  'name', coalesce(et.typname, bt.typname, ty.typname),
  'kind', coalesce(et.typtype, bt.typtype, ty.typtype)::text,
  'dims', CASE WHEN et.typname IS NOT NULL THEN 1 ELSE 0 END,
  'display', pg_catalog.format_type(ty.oid, NULL)
)), '[]')::text
FROM pg_catalog.pg_type ty
JOIN pg_catalog.pg_namespace tn ON tn.oid = ty.typnamespace
LEFT JOIN (SELECT t.oid, t.typname, t.typtype, n.nspname AS ns
             FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace) bt
  ON ty.typtype = 'd' AND bt.oid = ty.typbasetype
LEFT JOIN (SELECT t.oid, t.typname, t.typtype, n.nspname AS ns
             FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace) et
  ON ty.typcategory = 'A' AND ty.typelem <> 0 AND et.oid = ty.typelem
WHERE ty.oid IN (SELECT value::oid FROM json_array_elements_text($1::text::json))
"#;

#[derive(Deserialize)]
struct RawType {
    oid: u32,
    schema: String,
    name: String,
    kind: String,
    dims: u32,
    display: String,
}

pub(crate) async fn validate(
    client: &Client,
    type_names: &[String],
    checks: &[NativeQueryCheck],
) -> Result<(HashMap<String, NamedType>, CheckResults), Error> {
    let run = |sql: &'static str| async move {
        client
            .batch_execute(sql)
            .await
            .map_err(|e| pg_error(e, sql))
    };
    run("BEGIN READ ONLY").await?;
    let result = validate_in_transaction(client, type_names, checks).await;
    run("ROLLBACK").await?;
    result
}

async fn validate_in_transaction(
    client: &Client,
    type_names: &[String],
    checks: &[NativeQueryCheck],
) -> Result<(HashMap<String, NamedType>, CheckResults), Error> {
    // Every name, including the parameter types of the checks.
    let mut names: Vec<&String> = type_names.iter().collect();
    names.extend(checks.iter().flat_map(|c| &c.param_types));
    let mut oids: HashMap<String, u32> = HashMap::new();
    for name in names {
        if oids.contains_key(name) {
            continue;
        }
        // `to_regtype` returns NULL for unknown types, but older servers raise on syntax
        // errors in the name.
        let oid = savepoint(client, async {
            client
                .query_one("SELECT to_regtype($1::text)::oid", &[name])
                .await
                .and_then(|row| row.try_get::<_, Option<u32>>(0))
        })
        .await?;
        if let Ok(Some(oid)) = oid {
            oids.insert(name.clone(), oid);
        }
    }

    let mut results = CheckResults::new();
    for check in checks {
        let mut params = Vec::with_capacity(check.param_types.len());
        let mut unknown = None;
        for name in &check.param_types {
            match oids.get(name) {
                Some(oid) => params.push(param_type(*oid)),
                None => {
                    unknown = Some(name);
                    break;
                }
            }
        }
        if let Some(name) = unknown {
            let error = error_object(
                "42704",
                &format!("type \"{name}\" does not exist"),
                None,
                None,
            );
            results.insert(check.clone(), Err(error));
            continue;
        }
        let prepared = savepoint(client, client.prepare_typed(&check.sql, &params)).await?;
        let outcome = match prepared {
            Ok(stmt) => Ok(stmt
                .columns()
                .iter()
                .map(|c| (c.name().to_owned(), c.type_().oid()))
                .collect::<Vec<_>>()),
            Err(e) => Err(match e.as_db_error() {
                Some(db) => error_object(db.code().code(), db.message(), db.hint(), db.detail()),
                // Not from the server (e.g. looking up a result column's type): this
                // query's failure only.
                None => error_object("XX000", &e.to_string(), None, None),
            }),
        };
        results.insert(
            check.clone(),
            outcome.map(|cols| {
                cols.into_iter()
                    .map(|(name, oid)| ResultColumn {
                        name,
                        oid,
                        display: String::new(),
                    })
                    .collect()
            }),
        );
    }

    // Describe every type seen.
    let mut wanted: HashSet<u32> = oids.values().copied().collect();
    for cols in results.values().flatten() {
        wanted.extend(cols.iter().map(|c| c.oid));
    }
    let described = describe_types(client, &wanted).await?;
    for cols in results.values_mut().flatten() {
        for col in cols {
            if let Some(t) = described.get(&col.oid) {
                col.display = t.display.clone();
            }
        }
    }
    let types = oids
        .into_iter()
        .filter_map(|(name, oid)| Some((name, described.get(&oid)?.clone())))
        .collect();
    Ok((types, results))
}

async fn describe_types(
    client: &Client,
    oids: &HashSet<u32>,
) -> Result<HashMap<u32, NamedType>, Error> {
    let param = serde_json::to_string(&oids.iter().collect::<Vec<_>>()).expect("serialisable");
    let row = client
        .query_one(TYPES_SQL, &[&param])
        .await
        .map_err(|e| pg_error(e, TYPES_SQL))?;
    let text: String = row.try_get(0).map_err(|e| pg_error(e, TYPES_SQL))?;
    let raw: Vec<RawType> = serde_json::from_str(&text)
        .map_err(|e| Error::unexpected(format!("could not parse type catalog: {e}")))?;
    Ok(raw
        .into_iter()
        .map(|t| {
            let ty = ColumnType {
                schema: t.schema,
                name: t.name,
                kind: crate::introspect::type_kind(&t.kind),
                dimensions: t.dims,
            };
            (
                t.oid,
                NamedType {
                    ty,
                    display: t.display,
                    oid: t.oid,
                },
            )
        })
        .collect())
}

/// Run `step` inside a savepoint, rolling back to it if the step failed so the transaction
/// stays usable. The outer error is for the savepoint commands themselves.
async fn savepoint<T>(
    client: &Client,
    step: impl Future<Output = Result<T, tokio_postgres::Error>>,
) -> Result<Result<T, tokio_postgres::Error>, Error> {
    const SAVE: &str = "SAVEPOINT hge_native_check";
    const BACK: &str = "ROLLBACK TO SAVEPOINT hge_native_check";
    const RELEASE: &str = "RELEASE SAVEPOINT hge_native_check";
    client
        .batch_execute(SAVE)
        .await
        .map_err(|e| pg_error(e, SAVE))?;
    let result = step.await;
    let end = if result.is_ok() { RELEASE } else { BACK };
    client
        .batch_execute(end)
        .await
        .map_err(|e| pg_error(e, end))?;
    Ok(result)
}

/// A parameter type for `prepare_typed`; only its oid is sent to the server.
fn param_type(oid: u32) -> Type {
    Type::from_oid(oid)
        .unwrap_or_else(|| Type::new(String::new(), oid, Kind::Simple, String::new()))
}

/// V2's `error` object of a failed statement.
fn error_object(code: &str, message: &str, hint: Option<&str>, detail: Option<&str>) -> Value {
    json!({
        "description": detail,
        "exec_status": "FatalError",
        "hint": hint,
        "message": message,
        "status_code": code,
    })
}
