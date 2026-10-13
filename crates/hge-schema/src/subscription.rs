//! `subscription_root`: the query root's table fields (as allowed by each permission's
//! `subscription_root_fields`) plus streaming fields (`<table>_stream`).

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{Error, QualifiedTable};
use hge_ir::{OrderDirection, StreamCursor, StreamSelect};
use indexmap::IndexMap;
use serde_json::Value as Json;

use crate::build::Builder;
use crate::lower::{Lowerer, non_negative, to_value};
use crate::model::*;
use crate::normalize::SelectedField;
use crate::permissions::SelectAccess;
use crate::scalars;
use crate::tables::TableInfo;

pub const SUBSCRIPTION_ROOT: &str = "subscription_root";
const CURSOR_ORDERING: &str = "cursor_ordering";

/// The select access of a role as seen from the subscription root: the query-root field
/// switches replaced by `subscription_root_fields`, so the query root's registration code
/// builds the subscription root unchanged.
pub(crate) fn subscription_access(
    access: &HashMap<(Arc<str>, QualifiedTable), SelectAccess>,
) -> HashMap<(Arc<str>, QualifiedTable), SelectAccess> {
    access
        .iter()
        .map(|(k, a)| {
            let mut a = a.clone();
            a.query_root_fields = a.subscription_root_fields;
            (k.clone(), a)
        })
        .collect()
}

/// `<table>_stream`, or the `custom_root_fields.select_stream` name.
pub fn stream_field_name(table: &TableInfo) -> String {
    table
        .entry
        .configuration
        .custom_root_fields
        .as_ref()
        .and_then(|c| c.select_stream.as_ref())
        .and_then(|f| f.name())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}_stream", table.gql_name))
}

impl Builder<'_> {
    /// `<table>_stream(batch_size:, cursor:, where:)` on the subscription root.
    pub(crate) fn add_table_stream_root_field(
        &mut self,
        table: &Arc<TableInfo>,
        root: &mut ObjectDef,
    ) {
        let Some(access) = self.access(table).cloned() else {
            return;
        };
        if access.columns.is_empty() || !access.query_root_fields.select_stream {
            return;
        }
        self.ensure_table_types(table);
        let cursor_input = self.ensure_stream_cursor_types(table, &access);
        let field = FieldDef::new(
            stream_field_name(table),
            TypeRef::nn_list_nn(table.type_name()),
            FieldKind::SelectStream(table.clone()),
        )
        .describe(format!(
            "fetch data from the table in a streaming manner: \"{}\"",
            table.display_name()
        ))
        .arg(
            InputValueDef::new("batch_size", TypeRef::nn("Int"))
                .describe("maximum number of rows returned in a single batch"),
        )
        .arg(
            InputValueDef::new("cursor", TypeRef::named(cursor_input).list().non_null())
                .describe("cursor to stream the results returned by the query"),
        )
        .arg(
            InputValueDef::new("where", TypeRef::named(table.bool_exp_name()))
                .describe("filter the rows returned"),
        );
        root.fields.insert(field.name.clone(), field);
    }

    /// `<table>_stream_cursor_input`, `<table>_stream_cursor_value_input` and
    /// `cursor_ordering`; returns the cursor input's name.
    fn ensure_stream_cursor_types(&mut self, table: &TableInfo, access: &SelectAccess) -> String {
        let input = format!("{}_stream_cursor_input", table.gql_name);
        if self.schema.types.contains_key(&input) {
            return input;
        }
        let value_input = format!("{}_stream_cursor_value_input", table.gql_name);
        let mut values = IndexMap::new();
        for col in table.columns.values() {
            if !access.allows_column(&col.name) {
                continue;
            }
            self.note_scalar(&col.ty);
            values.insert(
                col.gql_name.clone(),
                InputValueDef::new(
                    col.gql_name.clone(),
                    scalars::column_type_ref(&col.ty, true),
                )
                .describe_opt(col.description.clone()),
            );
        }
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: value_input.clone(),
            description: Some(
                "Initial value of the column from where the streaming should start".to_owned(),
            ),
            fields: values,
        }));
        let mut fields = IndexMap::new();
        fields.insert(
            "initial_value".to_owned(),
            InputValueDef::new("initial_value", TypeRef::nn(value_input))
                .describe("Stream column input with initial value"),
        );
        fields.insert(
            "ordering".to_owned(),
            InputValueDef::new("ordering", TypeRef::named(CURSOR_ORDERING))
                .describe("cursor ordering"),
        );
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: input.clone(),
            description: Some(format!(
                "Streaming cursor of the table \"{}\"",
                table.gql_name
            )),
            fields,
        }));
        self.ensure_cursor_ordering_enum();
        input
    }

    fn ensure_cursor_ordering_enum(&mut self) {
        if self.schema.types.contains_key(CURSOR_ORDERING) {
            return;
        }
        let value = |name: &str, description: &str| {
            (
                name.to_owned(),
                EnumValueDef {
                    name: name.to_owned(),
                    description: Some(description.to_owned()),
                    deprecation_reason: None,
                },
            )
        };
        self.schema.add(TypeDef::Enum(EnumDef {
            name: CURSOR_ORDERING.to_owned(),
            description: Some("ordering argument of a cursor".to_owned()),
            values: [
                value("ASC", "ascending ordering of the cursor"),
                value("DESC", "descending ordering of the cursor"),
            ]
            .into_iter()
            .collect(),
        }));
    }
}

/// Lower `<table>_stream(...)`.
pub(crate) fn lower_stream(
    lw: &Lowerer<'_>,
    table: &TableInfo,
    field: &SelectedField<'_>,
) -> Result<StreamSelect, Error> {
    let mut select = lw.select(table, field, false)?;
    if let Some(Json::Object(w)) = field.arg("where") {
        select.args.where_ = lw.bool_exp(table, w, &field.arg_path("where"))?;
    }
    let batch_size =
        non_negative(field.arg("batch_size"), &field.arg_path("batch_size"))?.unwrap_or_default();

    let cursor_path = field.arg_path("cursor");
    let items = match field.arg("cursor") {
        Some(Json::Array(items)) => items.iter().collect(),
        Some(Json::Null) | None => vec![],
        Some(other) => vec![other],
    };
    // Every (cursor item, column given in its initial value) pair; exactly one is allowed.
    let mut cursors = vec![];
    for item in items {
        let Json::Object(item) = item else { continue };
        let direction = match item.get("ordering").and_then(Json::as_str) {
            Some("DESC") => OrderDirection::Desc,
            _ => OrderDirection::Asc,
        };
        if let Some(Json::Object(initial)) = item.get("initial_value") {
            for (name, value) in initial {
                if value.is_null() {
                    continue;
                }
                cursors.push((name, value, direction));
            }
        }
    }
    let (name, value, direction) = match cursors.as_slice() {
        [] => {
            return Err(
                Error::validation("one streaming column field is expected").with_path(cursor_path)
            );
        }
        [one] => *one,
        _ => {
            return Err(
                Error::validation("multiple column cursors are not supported yet")
                    .with_path(cursor_path),
            );
        }
    };
    let col = table.column_by_gql_name(name).ok_or_else(|| {
        Error::validation(format!("unknown column '{name}'")).with_path(cursor_path.clone())
    })?;
    let initial_value = to_value(value, &col.ty, &cursor_path)?;
    Ok(StreamSelect {
        select,
        batch_size,
        cursor: StreamCursor {
            column: col.name.clone(),
            ty: col.ty.clone(),
            direction,
            initial_value,
            redact: lw.redaction(table, &col.name).cloned().map(Box::new),
        },
    })
}
