//! `hdb_catalog.hdb_action_log` in the metadata database: the Postgres [`ActionLogStore`].
//! The table itself is created with the rest of the catalog (`hge_metadata_api::PgCatalog`,
//! V2's `initialise.sql`, spec 06 §9.2).

use hge_actions::ActionLogStore;
use hge_actions::async_action::{ActionLogEntry, NewActionLog, PendingAction, UtcTimestamp};
use hge_core::Error;
use hge_ir::BoxFuture;
use hge_postgres::{PgSource, PoolConfig};
use serde_json::{Value, json};
use tracing::warn;

// Parameters are bound as text, so every one is cast explicitly.
const INSERT_SQL: &str = r#"INSERT INTO "hdb_catalog"."hdb_action_log"
    ("action_name", "session_variables", "request_headers", "input_payload", "status")
VALUES ($1::text, $2::text::jsonb, $3::text::jsonb, $4::text::jsonb, $5::text)
RETURNING "id"::text"#;

const CLAIM_SQL: &str = "update hdb_catalog.hdb_action_log set status = 'processing' \
where id in ( \
  select id from hdb_catalog.hdb_action_log \
  where status = 'created' \
  for update skip locked limit $1::text::int \
) \
returning id::text, action_name, request_headers::text, session_variables::text, input_payload::text";

const COMPLETE_SQL: &str = "update hdb_catalog.hdb_action_log \
set response_payload = $1::text::jsonb, status = 'completed' where id = $2::text::uuid";

const FAIL_SQL: &str = "update hdb_catalog.hdb_action_log \
set errors = $1::text::jsonb, status = 'error' where id = $2::text::uuid";

const RELEASE_SQL: &str = "UPDATE hdb_catalog.hdb_action_log SET status = 'created' \
WHERE status = 'processing' AND id = ANY($1::text::uuid[])";

const FETCH_SQL: &str = "SELECT id::text, \
to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US'), \
response_payload::text, errors::text, session_variables::text \
FROM hdb_catalog.hdb_action_log WHERE id = $1::text::uuid";

pub struct PgActionLog {
    db: PgSource,
}

impl PgActionLog {
    /// Connect to the metadata database. The catalog must be initialised already
    /// (`PgCatalog::init`). This is a pool of its own, sized for the processor's concurrent
    /// batch, next to the catalog store's small one.
    pub fn connect(url: &str, config: &PoolConfig) -> Result<Self, Error> {
        Ok(PgActionLog {
            db: PgSource::connect("hdb_catalog", url, config)?,
        })
    }
}

fn json(text: Option<String>, column: &str) -> Result<Value, Error> {
    let text = text.ok_or_else(|| Error::unexpected(format!("{column} is null")))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::unexpected(format!("invalid JSON in action log {column}: {e}")))
}

fn opt_json(text: Option<String>, column: &str) -> Result<Option<Value>, Error> {
    text.map(|t| json(Some(t), column)).transpose()
}

fn take(row: &mut [Option<String>], i: usize) -> Option<String> {
    row.get_mut(i).and_then(Option::take)
}

/// Decode the columns of a claimed row after its id.
fn pending(id: String, mut r: Vec<Option<String>>) -> Result<PendingAction, Error> {
    Ok(PendingAction {
        id,
        action_name: take(&mut r, 1),
        request_headers: json(take(&mut r, 2), "request_headers")?,
        session_variables: json(take(&mut r, 3), "session_variables")?,
        input_payload: json(take(&mut r, 4), "input_payload")?,
    })
}

impl ActionLogStore for PgActionLog {
    fn insert<'a>(&'a self, entry: NewActionLog<'a>) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            let session = entry.session_variables.to_string();
            let headers = entry.request_headers.to_string();
            let input = entry.input_payload.to_string();
            self.db
                .query_text(
                    INSERT_SQL,
                    &[
                        Some(entry.action_name),
                        Some(&session),
                        Some(&headers),
                        Some(&input),
                        Some("created"),
                    ],
                )
                .await?
                .ok_or_else(|| Error::unexpected("no id returned for the action log row"))
        })
    }

    fn claim(&self, limit: u32) -> BoxFuture<'_, Result<Vec<PendingAction>, Error>> {
        Box::pin(async move {
            let limit = limit.to_string();
            let rows = self.db.query_rows(CLAIM_SQL, &[Some(&limit)]).await?;
            let mut claimed = Vec::with_capacity(rows.len());
            // A row that cannot be decoded is marked `error` on its own, so it neither blocks
            // the rest of the batch nor stays `processing`.
            for mut r in rows {
                let Some(id) = take(&mut r, 0) else {
                    warn!(target: "async-actions", "claimed an action log row without an id");
                    continue;
                };
                match pending(id.clone(), r) {
                    Ok(p) => claimed.push(p),
                    Err(e) => {
                        warn!(target: "async-actions", %id, "invalid action log row: {e}");
                        let errors =
                            json!({"error": e.message, "path": "$", "code": e.code.as_str()});
                        if let Err(e) = self.fail(&id, &errors).await {
                            warn!(target: "async-actions", %id, "could not store the error: {e}");
                        }
                    }
                }
            }
            Ok(claimed)
        })
    }

    fn complete<'a>(
        &'a self,
        id: &'a str,
        response: &'a Value,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let response = response.to_string();
            self.db
                .query_rows(COMPLETE_SQL, &[Some(&response), Some(id)])
                .await
                .map(drop)
        })
    }

    fn fail<'a>(&'a self, id: &'a str, errors: &'a Value) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let errors = errors.to_string();
            self.db
                .query_rows(FAIL_SQL, &[Some(&errors), Some(id)])
                .await
                .map(drop)
        })
    }

    fn release<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            // Ids come from the log (`id::text`), so they need no quoting in the array literal.
            let array = format!("{{{}}}", ids.join(","));
            self.db
                .query_rows(RELEASE_SQL, &[Some(&array)])
                .await
                .map(drop)
        })
    }

    fn fetch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<ActionLogEntry>, Error>> {
        Box::pin(async move {
            let Some(mut r) = self.db.query_row(FETCH_SQL, &[Some(id)]).await? else {
                return Ok(None);
            };
            let created_at = take(&mut r, 1)
                .as_deref()
                .and_then(UtcTimestamp::parse)
                .ok_or_else(|| Error::unexpected("invalid created_at in the action log"))?;
            Ok(Some(ActionLogEntry {
                id: take(&mut r, 0).unwrap_or_default(),
                created_at,
                response_payload: opt_json(take(&mut r, 2), "response_payload")?,
                errors: opt_json(take(&mut r, 3), "errors")?,
                session_variables: json(take(&mut r, 4), "session_variables")?,
            }))
        })
    }
}
