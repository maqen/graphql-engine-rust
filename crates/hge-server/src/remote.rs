//! Remote schemas in the server: introspecting them when metadata changes, and executing
//! remote root fields.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use hge_core::{InconsistentObject, RoleName};
use hge_ir::{NamespaceItem, RemoteRootField, RemoteRootKind};
use hge_metadata::Metadata;
use hge_metadata_api::Reload;
use hge_remote_schema::{
    RemoteError, RemoteResponse, RemoteSchemaClient, RemoteSettings, RequestContext,
};
use hge_schema::remote::{RemoteSchemaInfo, RemoteSchemas, check_conflicts, metadata};
use hge_schema::{ResolvedActions, SchemaOptions, Tables, build_role_schema};
use serde_json::{Map, Value as Json};

use crate::config::ServerConfig;

/// An introspected remote schema.
#[derive(Clone)]
pub struct PreparedRemote {
    /// The metadata entry it was built from; a change means a new introspection.
    key: String,
    pub client: Arc<RemoteSchemaClient>,
    /// The remote's response to the introspection query, verbatim.
    pub raw_introspection: Arc<str>,
    pub info: Arc<RemoteSchemaInfo>,
}

/// One HTTP client for all remote schemas (connection reuse).
fn http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .use_rustls_tls()
                .build()
                .expect("HTTP client")
        })
        .clone()
}

fn inconsistency(entry: &Json, name: &str, reason: impl Into<String>) -> InconsistentObject {
    InconsistentObject::new(
        "remote_schema",
        format!("remote_schema {name}"),
        reason,
        entry.clone(),
    )
}

/// Introspect the metadata's remote schemas (reusing `base`'s results for unchanged ones
/// unless `reload` asks for re-introspection) and check them against the schema of the
/// tables and actions.
/// Returns every introspected remote, the ones accepted into the schema, and the
/// inconsistencies.
pub async fn prepare(
    metadata: &Metadata,
    config: &ServerConfig,
    base: Option<&HashMap<String, PreparedRemote>>,
    reload: &Reload,
    tables: &Arc<Tables>,
    actions: &ResolvedActions,
    options: &SchemaOptions,
) -> (
    HashMap<String, PreparedRemote>,
    RemoteSchemas,
    Vec<InconsistentObject>,
) {
    let mut inconsistencies = vec![];
    let reintrospect = matches!(reload, Reload::All | Reload::Introspect);
    let user_agent = format!("hasura-graphql-engine/{}", crate::routes::VERSION);
    let overrides = config.env_overrides.clone();
    let env: hge_remote_schema::EnvLookup = Arc::new(move |name: &str| {
        overrides
            .get(name)
            .cloned()
            .or_else(|| hge_remote_schema::process_env(name))
    });

    let tasks = metadata.remote_schemas.iter().map(|entry| {
        let user_agent = &user_agent;
        let env = env.clone();
        async move {
            let name = metadata::entry_name(entry);
            let parsed = metadata::parse(entry)
                .and_then(|m| m.definition.validate().map(|()| m))
                .map_err(|e| inconsistency(entry, &name, e))?;
            let key = entry.to_string();
            if let Some(prev) = base
                .and_then(|b| b.get(&name))
                .filter(|p| p.key == key && !reintrospect)
            {
                return Ok(prev.clone());
            }
            let client = RemoteSchemaClient::new(
                &name,
                parsed.definition.clone(),
                http_client(),
                user_agent,
                env,
            );
            let (raw, json) = client
                .introspect()
                .await
                .map_err(|e| inconsistency(entry, &name, e.message))?;
            let info =
                RemoteSchemaInfo::new(&name, parsed.definition.customization.as_ref(), &json)
                    .map_err(|e| inconsistency(entry, &name, e))?;
            Ok::<_, InconsistentObject>(PreparedRemote {
                key,
                client: Arc::new(client),
                raw_introspection: raw.into(),
                info: Arc::new(info),
            })
        }
    });
    let mut prepared = HashMap::new();
    let mut ordered = vec![];
    for result in futures::future::join_all(tasks).await {
        match result {
            Ok(p) => {
                ordered.push(p.info.clone());
                prepared.insert(p.client.name.to_string(), p);
            }
            Err(issue) => inconsistencies.push(issue),
        }
    }
    if ordered.is_empty() {
        return (prepared, RemoteSchemas::default(), inconsistencies);
    }
    let admin = build_role_schema(
        tables,
        actions,
        &RemoteSchemas::default(),
        &RoleName::admin(),
        true,
        options,
    );
    let (accepted, rejected) = check_conflicts(&admin.schema, ordered);
    let position = |name: &str| {
        metadata
            .remote_schemas
            .iter()
            .position(|e| metadata::entry_name(e) == name)
    };
    for (name, reasons) in rejected {
        let entry = metadata
            .remote_schemas
            .iter()
            .find(|e| metadata::entry_name(e) == *name)
            .cloned()
            .unwrap_or_default();
        inconsistencies.push(inconsistency(&entry, &name, reasons.join(", ")));
        prepared.remove(&*name);
    }
    // Report in metadata order.
    inconsistencies.sort_by_key(|o| position(o.name.trim_start_matches("remote_schema ")));
    (prepared, accepted, inconsistencies)
}

/// Run a remote root field; returns its value as JSON text. The calls of a namespace field
/// run concurrently, or one after another (stopping at the first error) when `sequential`
/// (mutations).
pub async fn execute(
    clients: &HashMap<String, PreparedRemote>,
    field: &RemoteRootField,
    ctx: &RequestContext<'_>,
    settings: RemoteSettings,
    sequential: bool,
    set_cookies: &mut Vec<String>,
) -> Result<String, RemoteError> {
    let client = &clients
        .get(&*field.remote_schema)
        .ok_or_else(|| {
            hge_core::Error::unexpected(format!(
                "remote schema \"{}\" is not available",
                field.remote_schema
            ))
        })?
        .client;
    match &field.kind {
        RemoteRootKind::Field(call) => {
            let RemoteResponse {
                value,
                set_cookies: cookies,
            } = client.call(call, ctx, settings).await?;
            set_cookies.extend(cookies);
            Ok(value.to_string())
        }
        RemoteRootKind::Namespace(items) => {
            let run = |item| run_item(client, item, ctx, settings);
            let mut out = Map::new();
            if sequential {
                for (key, item) in items {
                    let r = run(item).await?;
                    set_cookies.extend(r.set_cookies);
                    out.insert(key.clone(), r.value);
                }
            } else {
                let results = futures::future::join_all(items.iter().map(|(_, i)| run(i))).await;
                for ((key, _), result) in items.iter().zip(results) {
                    let r = result?;
                    set_cookies.extend(r.set_cookies);
                    out.insert(key.clone(), r.value);
                }
            }
            Ok(Json::Object(out).to_string())
        }
    }
}

async fn run_item(
    client: &RemoteSchemaClient,
    item: &NamespaceItem,
    ctx: &RequestContext<'_>,
    settings: RemoteSettings,
) -> Result<RemoteResponse, RemoteError> {
    match item {
        NamespaceItem::Static(v) => Ok(RemoteResponse {
            value: v.clone(),
            set_cookies: vec![],
        }),
        NamespaceItem::Call(call) => client.call(call, ctx, settings).await,
    }
}
