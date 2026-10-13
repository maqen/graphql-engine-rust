//! Resolving `rest_endpoints` against the query collections (V2's `buildRESTEndpoints`).

use std::collections::HashSet;

use async_graphql_parser::types::{
    BaseType, DocumentOperations, ExecutableDocument, OperationType, Type,
};
use hge_core::InconsistentObject;
use hge_metadata::{Metadata, RestEndpoint};
use serde_json::{Value, json};

use crate::{Endpoint, RestEndpoints, Segment, VarDef, parse_url};

/// The routable endpoints of `metadata` and the inconsistencies V2 reports for them.
///
/// Endpoints whose query cannot be resolved to one typed operation are left out. Endpoints
/// with a malformed or ambiguous URL are reported but still routed, as in V2 (an ambiguous
/// request then fails with `Multiple endpoints match request`).
pub fn build(metadata: &Metadata) -> (RestEndpoints, Vec<InconsistentObject>) {
    let mut routable: Vec<(&RestEndpoint, Endpoint)> = vec![];
    let mut definition_issues = vec![];
    for e in &metadata.rest_endpoints {
        match resolve(metadata, e) {
            Ok(endpoint) => routable.push((e, endpoint)),
            Err(reason) => definition_issues.push(endpoint_issue(
                e,
                format!("in endpoint {}: {reason}", e.name),
            )),
        }
    }

    let mut issues = vec![];
    // V2 reports every duplicate-variable error, then every empty-segment error.
    for (e, endpoint) in &routable {
        if has_duplicate_vars(&endpoint.segments) {
            let reason = format!("Duplicate variables found in endpoint path: {}", e.url);
            issues.push(endpoint_issue(e, reason).bare());
        }
    }
    for (e, endpoint) in &routable {
        if has_empty_segment(&endpoint.segments) {
            let reason = format!(
                "Empty segments or unnamed variables are not allowed: {}",
                e.url
            );
            issues.push(endpoint_issue(e, reason).bare());
        }
    }
    issues.extend(ambiguities(&routable));
    issues.extend(definition_issues);

    let mut endpoints = RestEndpoints::default();
    for (_, endpoint) in routable {
        endpoints.insert(endpoint);
    }
    (endpoints, issues)
}

fn endpoint_issue(e: &RestEndpoint, reason: String) -> InconsistentObject {
    InconsistentObject::new(
        "rest_endpoint",
        format!("rest_endpoint {}", e.name),
        reason,
        serde_json::to_value(e).unwrap_or(Value::Null),
    )
}

fn resolve(metadata: &Metadata, e: &RestEndpoint) -> Result<Endpoint, String> {
    let r = &e.definition.query;
    let collection = metadata
        .query_collections
        .iter()
        .find(|c| c.name == r.collection_name)
        .ok_or_else(|| format!("collection with name {} does not exist", r.collection_name))?;
    let query = collection.query(&r.query_name).ok_or_else(|| {
        format!(
            "query with name {} does not exist in collection {}",
            r.query_name, r.collection_name
        )
    })?;
    let (document, variables) = single_typed_operation(&query.query).map_err(|issue| {
        let what = match issue {
            DefinitionIssue::Multiple => "has multiple definitions.",
            DefinitionIssue::Subscription => "is a subscription",
        };
        format!("query with name {} {what}", r.query_name)
    })?;
    Ok(Endpoint {
        name: e.name.clone(),
        url: e.url.clone(),
        segments: parse_url(&e.url),
        methods: e.methods.clone(),
        query: query.query.clone(),
        document,
        variables,
    })
}

enum DefinitionIssue {
    /// Anything but exactly one typed operation (V2's `_` case, which also covers the
    /// shorthand `{ … }` form and fragments).
    Multiple,
    Subscription,
}

fn single_typed_operation(
    text: &str,
) -> Result<(ExecutableDocument, Vec<VarDef>), DefinitionIssue> {
    let doc = async_graphql_parser::parse_query(text).map_err(|_| DefinitionIssue::Multiple)?;
    let op = match &doc.operations {
        DocumentOperations::Single(op) => op,
        DocumentOperations::Multiple(ops) if ops.len() == 1 => {
            ops.values().next().expect("one operation")
        }
        DocumentOperations::Multiple(_) => return Err(DefinitionIssue::Multiple),
    };
    if !doc.fragments.is_empty() || is_shorthand(text) {
        return Err(DefinitionIssue::Multiple);
    }
    if op.node.ty == OperationType::Subscription {
        return Err(DefinitionIssue::Subscription);
    }
    let variables = op
        .node
        .variable_definitions
        .iter()
        .map(|v| var_def(v.node.name.node.as_str(), &v.node.var_type.node))
        .collect();
    Ok((doc, variables))
}

fn var_def(name: &str, ty: &Type) -> VarDef {
    let mut inner = ty;
    while let BaseType::List(t) = &inner.base {
        inner = t;
    }
    let BaseType::Named(type_name) = &inner.base else {
        unreachable!("loop stops at a named type")
    };
    VarDef {
        name: name.to_owned(),
        type_name: type_name.to_string(),
        list: matches!(ty.base, BaseType::List(_)),
        nullable: ty.nullable,
    }
}

/// Whether a document holding exactly one operation and no fragments is the shorthand
/// `{ … }` form: its first token is `{`.
fn is_shorthand(text: &str) -> bool {
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',' || c == '\u{feff}');
        match rest.strip_prefix('#') {
            Some(comment) => rest = comment.find('\n').map_or("", |i| &comment[i..]),
            None => return rest.starts_with('{'),
        }
    }
}

fn has_empty_segment(segments: &[Segment]) -> bool {
    segments
        .iter()
        .any(|s| matches!(s, Segment::Literal(l) | Segment::Var(l) if l.is_empty()))
}

fn has_duplicate_vars(segments: &[Segment]) -> bool {
    let mut seen = HashSet::new();
    segments
        .iter()
        .filter_map(|s| match s {
            Segment::Var(name) => Some(name),
            Segment::Literal(_) => None,
        })
        .any(|name| !seen.insert(name))
}

/// Two endpoints conflict when they share a method and some URL matches both. Conflicting
/// endpoints are grouped transitively; each group is one inconsistency.
fn ambiguities(routable: &[(&RestEndpoint, Endpoint)]) -> Vec<InconsistentObject> {
    let n = routable.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut conflicting = vec![false; n];
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (&routable[i].1, &routable[j].1);
            if a.methods.iter().any(|m| b.methods.contains(m)) && overlap(&a.segments, &b.segments)
            {
                conflicting[i] = true;
                conflicting[j] = true;
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut group_of_root: std::collections::HashMap<usize, usize> = Default::default();
    for i in (0..n).filter(|&i| conflicting[i]) {
        let r = root(&mut parent, i);
        let g = *group_of_root.entry(r).or_insert_with(|| {
            groups.push(vec![]);
            groups.len() - 1
        });
        groups[g].push(i);
    }
    groups
        .into_iter()
        .map(|mut members| {
            members.sort_by(|&a, &b| routable[a].0.name.cmp(&routable[b].0.name));
            let urls: Vec<&str> = members
                .iter()
                .map(|&i| routable[i].0.url.as_str())
                .collect();
            let conflicts = members
                .iter()
                .map(|&i| {
                    let (e, endpoint) = &routable[i];
                    let json = json!({
                        "comment": e.comment,
                        "definition": {"query": endpoint.query},
                        "methods": e.methods,
                        "name": e.name,
                        "url": e.url,
                    });
                    (e.name.clone(), json)
                })
                .collect();
            InconsistentObject::ambiguous_rest_endpoints(
                format!("Ambiguous URL paths: {}", urls.join(", ")),
                conflicts,
            )
        })
        .collect()
}

/// Whether some concrete path matches both URL templates.
fn overlap(a: &[Segment], b: &[Segment]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|pair| match pair {
            (Segment::Literal(x), Segment::Literal(y)) => x == y,
            _ => true,
        })
}
