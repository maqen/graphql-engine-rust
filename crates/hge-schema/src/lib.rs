//! GraphQL schema generation (per role) and request processing.
//!
//! Pipeline: [`resolve_tables`] (metadata + catalogs, once per metadata version) →
//! [`build_role_schema`] (once per role) → per request: [`parse_query`] → [`plan`]
//! (validate, normalise, lower to IR).

pub mod build;
pub mod geojson;
pub mod introspection;
pub mod lower;
pub mod model;
pub mod mutation;
pub mod normalize;
pub mod permissions;
pub mod scalars;
pub mod tables;

use std::collections::HashMap;

use async_graphql_parser::types::ExecutableDocument;
use hge_core::{Error, InconsistentObject, SourceCatalog};
use hge_ir::Operation;
use hge_metadata::Metadata;
use serde_json::{Map, Value as Json};

pub use build::{RoleSchema, SchemaOptions, build_role_schema};
pub use tables::{TableInfo, Tables};

/// Join metadata with the database catalogs. Returns the tables and a list of
/// inconsistencies (objects that could not be resolved and were skipped).
pub fn resolve_tables<C: std::borrow::Borrow<SourceCatalog>>(
    metadata: &Metadata,
    catalogs: &HashMap<String, C>,
) -> (Tables, Vec<InconsistentObject>) {
    let mut tables = Tables::default();
    let mut inconsistencies = vec![];
    for source in &metadata.sources {
        // A source without a catalog is itself reported inconsistent by whoever failed to
        // introspect it; like V2, its tables are not listed separately.
        if let Some(catalog) = catalogs.get(&source.name) {
            tables::resolve_source(source, catalog.borrow(), &mut tables, &mut inconsistencies)
        }
    }
    tables::drop_conflicting(&mut tables, &mut inconsistencies);
    permissions::resolve_permissions(&mut tables, &mut inconsistencies);
    // V2 lists sources, then tables, then the objects inside tables.
    inconsistencies.sort_by_key(|o| match o.kind.as_str() {
        "source" => 0,
        "table" => 1,
        _ => 2,
    });
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
