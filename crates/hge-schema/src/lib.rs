//! GraphQL schema generation (per role) and request processing.
//!
//! Pipeline: [`resolve_tables`] (metadata + catalogs, once per metadata version) →
//! [`build_role_schema`] (once per role) → per request: [`parse_query`] → [`plan`]
//! (validate, normalise, lower to IR).

pub mod build;
pub mod introspection;
pub mod lower;
pub mod model;
pub mod normalize;
pub mod permissions;
pub mod scalars;
pub mod tables;

use std::collections::HashMap;

use async_graphql_parser::types::ExecutableDocument;
use hge_core::{Error, SourceCatalog};
use hge_ir::Operation;
use hge_metadata::Metadata;
use serde_json::{Map, Value as Json};

pub use build::{RoleSchema, SchemaOptions, build_role_schema};
pub use tables::{TableInfo, Tables};

/// Join metadata with the database catalogs. Returns the tables and a list of
/// inconsistencies (objects that could not be resolved and were skipped).
pub fn resolve_tables(
    metadata: &Metadata,
    catalogs: &HashMap<String, SourceCatalog>,
) -> (Tables, Vec<String>) {
    let mut tables = Tables::default();
    let mut inconsistencies = vec![];
    for source in &metadata.sources {
        match catalogs.get(&source.name) {
            Some(catalog) => {
                tables::resolve_source(source, catalog, &mut tables, &mut inconsistencies)
            }
            None => inconsistencies.push(format!(
                "source \"{}\" is not reachable or not supported",
                source.name
            )),
        }
    }
    tables::drop_conflicting(&mut tables, &mut inconsistencies);
    (tables, inconsistencies)
}

/// Parse a GraphQL query document.
pub fn parse_query(query: &str) -> Result<ExecutableDocument, Error> {
    // V2 gives no parser details in the response.
    async_graphql_parser::parse_query(query)
        .map_err(|_| Error::validation("not a valid graphql query").with_path("$.query"))
}

/// Validate a request against the role's schema and lower it to IR.
pub fn plan(
    rs: &RoleSchema,
    doc: &ExecutableDocument,
    operation_name: Option<&str>,
    variables: &Map<String, Json>,
) -> Result<Operation, Error> {
    let op = normalize::normalize(&rs.schema, doc, operation_name, variables)?;
    lower::lower(rs, &op)
}
