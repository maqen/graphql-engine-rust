//! RESTified endpoints (`/api/rest/<url>`, spec 13 §3 and spec 10 §5).
//!
//! [`RestEndpoints::build`] resolves the metadata's `rest_endpoints` against its query
//! collections once per metadata version and reports V2's inconsistencies;
//! [`RestEndpoints::lookup`] matches a request and [`Endpoint::variables`] turns its query
//! string, JSON body and URL segments into GraphQL variables. Pure: no IO, no async.

mod build;
mod variables;

use std::collections::HashMap;

use async_graphql_parser::types::ExecutableDocument;
use hge_core::{Code, Error};
use hge_metadata::EndpointMethod;

pub use build::build;
pub use variables::{QueryParams, parse_query_string};

/// Decoded values of an endpoint's `:name` URL segments, in URL order.
pub type PathVars = Vec<(String, String)>;

/// One segment of an endpoint URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    /// `:name` (the name may be empty in an inconsistent endpoint, which still routes).
    Var(String),
}

/// A declared variable of the stored operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDef {
    pub name: String,
    /// The innermost named type.
    pub type_name: String,
    /// The outer type is a list.
    pub list: bool,
    pub nullable: bool,
}

/// An endpoint whose query resolved to one typed operation.
#[derive(Debug)]
pub struct Endpoint {
    pub name: String,
    pub url: String,
    pub segments: Vec<Segment>,
    pub methods: Vec<EndpointMethod>,
    /// The stored query text.
    pub query: String,
    /// `query`, parsed once.
    pub document: ExecutableDocument,
    pub variables: Vec<VarDef>,
}

/// The routable endpoints of one metadata version.
#[derive(Debug, Default)]
pub struct RestEndpoints {
    /// Endpoints by number of URL segments.
    by_len: HashMap<usize, Vec<Endpoint>>,
}

/// A request method as the REST route sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMethod<'a> {
    Endpoint(EndpointMethod),
    /// A standard method REST endpoints cannot have (`HEAD`, `OPTIONS`, …).
    Unsupported(&'a str),
    Nonstandard,
}

impl<'a> RequestMethod<'a> {
    pub fn parse(method: &'a str) -> Self {
        if let Some(m) = EndpointMethod::parse(method) {
            return RequestMethod::Endpoint(m);
        }
        match method {
            "HEAD" | "OPTIONS" | "CONNECT" | "TRACE" => RequestMethod::Unsupported(method),
            _ => RequestMethod::Nonstandard,
        }
    }
}

impl RestEndpoints {
    pub fn is_empty(&self) -> bool {
        self.by_len.is_empty()
    }

    /// Match `path` (the raw, still percent-encoded part after `/api/rest/`). Returns the
    /// endpoint and its URL variables (decoded), or V2's routing error.
    pub fn lookup(
        &self,
        method: RequestMethod<'_>,
        path: &str,
    ) -> Result<(&Endpoint, PathVars), Error> {
        let method = match method {
            RequestMethod::Endpoint(m) => m,
            RequestMethod::Unsupported(m) => {
                return Err(Error::new(
                    Code::BadRequest,
                    format!("Method {m} not supported."),
                ));
            }
            RequestMethod::Nonstandard => {
                return Err(Error::new(
                    Code::BadRequest,
                    "Nonstandard method not allowed for REST endpoints",
                ));
            }
        };
        // Like WAI's `pathInfo`, a trailing slash does not add an empty segment.
        let parts: Vec<String> = path
            .trim_end_matches('/')
            .split('/')
            .map(|p| variables::percent_decode(p, false))
            .collect();
        let candidates = self
            .by_len
            .get(&parts.len())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut matched: Vec<&Endpoint> = candidates
            .iter()
            .filter(|e| {
                e.segments.iter().zip(&parts).all(|(s, p)| match s {
                    Segment::Literal(l) => l == p,
                    Segment::Var(_) => !p.is_empty(),
                })
            })
            .collect();
        if matched.is_empty() {
            return Err(Error::new(Code::NotFound, "Endpoint not found"));
        }
        let all_methods: Vec<EndpointMethod> = {
            let mut ms = vec![];
            for m in matched.iter().flat_map(|e| &e.methods) {
                if !ms.contains(m) {
                    ms.push(*m);
                }
            }
            ms
        };
        matched.retain(|e| e.methods.contains(&method));
        match matched.as_slice() {
            [] => Err(Error::new(
                Code::MethodNotAllowed,
                format!(
                    "Allowed methods: {}",
                    all_methods
                        .iter()
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
            [endpoint] => {
                let vars = endpoint
                    .segments
                    .iter()
                    .zip(&parts)
                    .filter_map(|(s, p)| match s {
                        Segment::Var(name) => Some((name.clone(), p.clone())),
                        Segment::Literal(_) => None,
                    })
                    .collect();
                Ok((endpoint, vars))
            }
            _ => Err(Error::unexpected("Multiple endpoints match request")),
        }
    }

    pub(crate) fn insert(&mut self, endpoint: Endpoint) {
        self.by_len
            .entry(endpoint.segments.len())
            .or_default()
            .push(endpoint);
    }
}

/// Split an endpoint URL into segments (V2's `splitPath`).
pub fn parse_url(url: &str) -> Vec<Segment> {
    url.split('/')
        .map(|s| match s.strip_prefix(':') {
            Some(name) => Segment::Var(name.to_owned()),
            None => Segment::Literal(s.to_owned()),
        })
        .collect()
}

#[cfg(test)]
mod tests;
