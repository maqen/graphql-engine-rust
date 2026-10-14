//! Remote joins (spec 08 §9): the database returns each remote relationship's join fields
//! in its place; this collects the distinct values per relationship, sends one batched
//! request per relationship, and puts the results back.

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::Error;
use hge_ir::{RemoteJoin, RemoteJoinTarget};
use hge_remote_schema::RequestContext;
use serde_json::{Map, Value as Json};

use crate::remote::PreparedRemote;

/// Walks a result along the joins of the IR that produced it.
pub type Visit<'a> = &'a (dyn Fn(&mut Json, &mut dyn FnMut(&RemoteJoin, &mut Json)) + Sync);

/// One relationship's distinct join values.
struct Join {
    target: Arc<dyn RemoteJoinTarget>,
    /// `(argument id, join values)`, ids in increasing order.
    arguments: Vec<(usize, Map<String, Json>)>,
    by_value: HashMap<String, usize>,
}

/// Resolve the remote joins of one root field's result (JSON text).
pub async fn resolve(
    remotes: &HashMap<String, PreparedRemote>,
    json: String,
    visit: Visit<'_>,
    ctx: &RequestContext<'_>,
) -> Result<String, Error> {
    let mut value: Json = serde_json::from_str(&json)
        .map_err(|e| Error::unexpected(format!("invalid database result: {e}")))?;

    // Collect the arguments; each slot remembers which result goes there (`None`: null).
    let mut joins: Vec<Join> = vec![];
    let mut join_of: HashMap<usize, usize> = HashMap::new();
    let mut slots: Vec<Option<(usize, usize)>> = vec![];
    let mut next_id = 0;
    visit(&mut value, &mut |join, slot| {
        let Json::Object(values) = slot else {
            slots.push(None);
            return;
        };
        // A null join value makes the relationship null (spec 08 §9.3).
        if values.values().any(Json::is_null) {
            slots.push(None);
            return;
        }
        let key = Arc::as_ptr(&join.target) as *const () as usize;
        let j = *join_of.entry(key).or_insert_with(|| {
            joins.push(Join {
                target: join.target.clone(),
                arguments: vec![],
                by_value: HashMap::new(),
            });
            joins.len() - 1
        });
        let join = &mut joins[j];
        let text = Json::Object(values.clone()).to_string();
        let id = *join.by_value.entry(text).or_insert_with(|| {
            join.arguments.push((next_id, values.clone()));
            next_id += 1;
            next_id - 1
        });
        slots.push(Some((j, id)));
    });
    if joins.is_empty() && slots.is_empty() {
        return Ok(json);
    }

    // One request per relationship, all at once; the first failing one (in join order)
    // decides the error.
    let calls = joins.iter().map(|join| async move {
        let client = remotes
            .get(join.target.remote_schema())
            .ok_or_else(|| {
                Error::unexpected(format!(
                    "remote schema \"{}\" is not available",
                    join.target.remote_schema()
                ))
            })?
            .client
            .clone();
        let args: Vec<(usize, &Map<String, Json>)> =
            join.arguments.iter().map(|(id, a)| (*id, a)).collect();
        let call = join.target.request(&args)?;
        let data = client.call_join(&call, ctx).await?;
        join.arguments
            .iter()
            .map(|(id, _)| Ok((*id, join.target.extract(&data, *id)?)))
            .collect::<Result<HashMap<usize, Json>, Error>>()
    });
    let mut results = vec![];
    for r in futures::future::join_all(calls).await {
        results.push(r?);
    }

    let mut slots = slots.into_iter();
    visit(&mut value, &mut |_, slot| {
        *slot = match slots.next().flatten() {
            Some((j, id)) => results[j].get(&id).cloned().unwrap_or(Json::Null),
            None => Json::Null,
        };
    });
    Ok(value.to_string())
}
