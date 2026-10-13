//! The Relay API (`/v1/relay`, `/v1beta1/relay`; V2's `Schema/Relay.hs`, `Schema/Node.hs`
//! and the connection parts of `Schema/Select.hs`).
//!
//! The Relay schema of a role is the normal one with these differences:
//!
//! * the query and subscription roots have `node(id:)` and, for every table with a primary
//!   key, `<table>_connection(...)` (and `<function>_connection(...)` for query functions
//!   returning such a table) instead of the usual select, aggregate and stream fields;
//! * tables with a primary key implement the `Node` interface: their `id: ID!` field is a
//!   global id (base64 of `[1, "<schema>", "<table>", <key values>…]`), and a column named
//!   `id` is hidden on every table;
//! * array relationships to tables with a primary key also get `<rel>_connection(...)`;
//! * the mutation root has the table and function mutations and action mutations only.

pub(crate) mod lower;
mod node_id;

use std::sync::Arc;

use hge_core::RoleName;
use indexmap::IndexMap;

use crate::actions::ResolvedActions;
use crate::build::{Builder, RoleSchema, SchemaOptions};
use crate::functions::{ExposedAs, FunctionInfo};
use crate::model::*;
use crate::remote::RemoteSchemas;
use crate::tables::{RelationshipInfo, TableInfo, Tables};

pub const NODE: &str = "Node";
pub const PAGE_INFO: &str = "PageInfo";

/// V2 lists `order_by` twice among a connection's arguments (it parses it twice); the
/// second copy is stored under this key so that introspection can show it.
pub(crate) const ORDER_BY_COPY: &str = "order_by#relay";

/// A field of a connection, edge or `PageInfo` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPart {
    PageInfo,
    Edges,
    Cursor,
    EdgeNode,
    StartCursor,
    EndCursor,
    HasNextPage,
    HasPreviousPage,
}

/// The schema of `role` for the Relay endpoint (see [`crate::build_role_schema`]). Like V2,
/// it has no remote schema fields: `_remotes` is taken only to match `build_role_schema`.
pub fn build_relay_role_schema(
    tables: &Arc<Tables>,
    actions: &ResolvedActions,
    _remotes: &RemoteSchemas,
    role: &RoleName,
    backend_only: bool,
    options: &SchemaOptions,
) -> RoleSchema {
    let none = RemoteSchemas::default();
    crate::build::build_role_schema_of_kind(tables, actions, &none, role, backend_only, options, true)
}

/// Whether `table` implements `Node` in the Relay schema (it needs a primary key; the role's
/// select access is checked by the caller).
fn is_node(table: &TableInfo) -> bool {
    table.primary_key.is_some() && !table.logical_model
}

/// `node` and the connection root fields, on the query or subscription root.
pub(crate) fn add_query_root_fields(
    b: &mut Builder<'_>,
    _role: &RoleName,
    _options: &SchemaOptions,
    root: &mut ObjectDef,
) {
    b.ensure_node_interface();
    root.fields.insert(
        "node".to_owned(),
        FieldDef::new("node", TypeRef::named(NODE), FieldKind::Node).arg(
            InputValueDef::new("id", TypeRef::nn("ID")).describe("A globally unique id"),
        ),
    );
    let tables = b.tables;
    for table in &tables.ordered {
        let Some(access) = b.access(table) else {
            continue;
        };
        if !is_node(table) || !access.query_root_fields.select {
            continue;
        }
        let field = b.connection_field(
            table,
            format!("{}_connection", table.gql_name),
            FieldKind::Connection(table.clone()),
        );
        root.fields.insert(
            field.name.clone(),
            field.describe(format!(
                "fetch data from the table: \"{}\"",
                table.display_name()
            )),
        );
    }
    for function in &tables.functions {
        if function.exposed_as != ExposedAs::Query {
            continue;
        }
        if let Some(field) = b.function_connection_field(function) {
            root.fields.insert(field.name.clone(), field);
        }
    }
}

impl Builder<'_> {
    /// The `Node` interface, its possible types being the tables the role can select from
    /// that have a primary key.
    pub(crate) fn ensure_node_interface(&mut self) {
        if self.schema.types.contains_key(NODE) {
            return;
        }
        let mut possible_types: Vec<String> = self
            .tables
            .ordered
            .iter()
            .filter(|t| is_node(t) && self.access(t).is_some())
            .map(|t| t.type_name().to_owned())
            .collect();
        possible_types.sort();
        possible_types.dedup();
        let id = FieldDef::new("id", TypeRef::nn("ID"), FieldKind::Introspection)
            .describe("A globally unique identifier");
        self.scalars.insert("ID".to_owned());
        self.schema.add(TypeDef::Interface(InterfaceDef {
            name: NODE.to_owned(),
            description: Some("An object with globally unique ID".to_owned()),
            fields: IndexMap::from([("id".to_owned(), id)]),
            possible_types,
        }));
        // Every possible type must be in the schema even when nothing else reaches it.
        let tables = self.tables;
        for t in &tables.ordered {
            if is_node(t) && self.access(t).is_some() {
                self.ensure_table_types(t);
            }
        }
    }

    /// The node id field of a Relay table, and `implements Node`.
    pub(crate) fn add_node_id(&mut self, table: &Arc<TableInfo>, obj: &mut ObjectDef) {
        if !self.relay || !is_node(table) {
            return;
        }
        self.scalars.insert("ID".to_owned());
        obj.fields.insert(
            "id".to_owned(),
            FieldDef::new("id", TypeRef::nn("ID"), FieldKind::NodeId(table.clone())),
        );
        obj.interfaces = vec![NODE.to_owned()];
    }

    /// `<rel>_connection` for an array relationship (Relay schema only).
    pub(crate) fn relationship_connection_field(
        &mut self,
        rel: &Arc<RelationshipInfo>,
        target: &Arc<TableInfo>,
    ) -> Option<FieldDef> {
        if !self.relay || !is_node(target) {
            return None;
        }
        let field = self.connection_field(
            target,
            format!("{}_connection", rel.name),
            FieldKind::ArrayRelationshipConnection(rel.clone()),
        );
        Some(field.describe("An array relationship connection"))
    }

    /// `<function>_connection(args:, …)` for a query function returning a table with a
    /// primary key.
    fn function_connection_field(&mut self, f: &Arc<FunctionInfo>) -> Option<FieldDef> {
        let table = self.tables.get(&f.source, &f.returns)?.clone();
        if !is_node(&table) || self.access(&table).is_none() {
            return None;
        }
        // V2 describes the `args` argument with the function's name, not the field's.
        let field = self.connection_field(
            &table,
            f.gql_name.clone(),
            FieldKind::FunctionConnection(f.clone()),
        );
        let mut field = self.with_function_args(field, f);
        field.name = format!("{}_connection", f.gql_name);
        Some(field.describe(format!(
            "execute function \"{}\" which returns \"{}\"",
            f.function.v2_text(),
            table.display_name()
        )))
    }

    /// A connection field on `table` (`<name>(after:, before:, distinct_on:, first:, last:,
    /// order_by:, where:): <table>Connection!`), with the connection types.
    fn connection_field(&mut self, table: &Arc<TableInfo>, name: String, kind: FieldKind) -> FieldDef {
        let ty = self.ensure_connection_types(table);
        let field = FieldDef::new(name, TypeRef::nn(ty), kind);
        let field = self.with_select_args(field, table);
        let mut field = field
            .arg(InputValueDef::new("after", TypeRef::named("String")))
            .arg(InputValueDef::new("before", TypeRef::named("String")))
            .arg(InputValueDef::new("first", TypeRef::named("Int")))
            .arg(InputValueDef::new("last", TypeRef::named("Int")));
        field.args.shift_remove("limit");
        field.args.shift_remove("offset");
        if let Some(order_by) = field.args.get("order_by").cloned() {
            field.args.insert(ORDER_BY_COPY.to_owned(), order_by);
        }
        field
    }

    /// `<table>Connection`, `<table>Edge` and `PageInfo`; returns the connection type name.
    fn ensure_connection_types(&mut self, table: &Arc<TableInfo>) -> String {
        let connection = format!("{}Connection", table.type_name());
        if self.schema.types.contains_key(&connection) {
            return connection;
        }
        self.ensure_table_types(table);
        let part = |name: &str, ty: TypeRef, part: ConnectionPart| {
            FieldDef::new(name, ty, FieldKind::ConnectionPart(part))
        };
        if !self.schema.types.contains_key(PAGE_INFO) {
            self.scalars.insert("Boolean".to_owned());
            self.scalars.insert("String".to_owned());
            self.schema.add(TypeDef::Object(
                ObjectDef::new(PAGE_INFO, None)
                    .field(part("endCursor", TypeRef::nn("String"), ConnectionPart::EndCursor))
                    .field(part(
                        "hasNextPage",
                        TypeRef::nn("Boolean"),
                        ConnectionPart::HasNextPage,
                    ))
                    .field(part(
                        "hasPreviousPage",
                        TypeRef::nn("Boolean"),
                        ConnectionPart::HasPreviousPage,
                    ))
                    .field(part(
                        "startCursor",
                        TypeRef::nn("String"),
                        ConnectionPart::StartCursor,
                    )),
            ));
        }
        let edge = format!("{}Edge", table.type_name());
        self.schema.add(TypeDef::Object(
            ObjectDef::new(edge.clone(), None)
                .field(part("cursor", TypeRef::nn("String"), ConnectionPart::Cursor))
                .field(part(
                    "node",
                    TypeRef::nn(table.type_name()),
                    ConnectionPart::EdgeNode,
                )),
        ));
        self.schema.add(TypeDef::Object(
            ObjectDef::new(
                connection.clone(),
                Some(format!(
                    "A Relay connection object on \"{}\"",
                    table.display_name()
                )),
            )
            .field(part("edges", TypeRef::nn_list_nn(edge), ConnectionPart::Edges))
            .field(part("pageInfo", TypeRef::nn(PAGE_INFO), ConnectionPart::PageInfo)),
        ));
        connection
    }
}
