//! Lowering `node` and connection fields to IR.

use hge_core::{Code, ColumnType, Error};
use hge_ir::{
    BoolExp, CompareOp, ConnectionField, ConnectionSelect, ConnectionSlice, ConnectionSplit,
    DatabaseRootField, EdgeField, NodeIdField, NullsOrder, OrderBy, OrderByTarget, OrderDirection,
    PageInfoField, QueryRoot, RootField, RowSource, Select, SplitItem, SplitKey, SplitKind, Value,
};
use serde_json::{Map, Value as Json};

use super::ConnectionPart;
use super::node_id::{Scalar, base64_decode_lenient, column_value, parse_node_id};
use crate::aggregate::aggregate_relationship;
use crate::functions::FunctionInfo;
use crate::lower::{Lowerer, non_negative};
use crate::model::FieldKind;
use crate::normalize::{Selected, SelectedField};
use crate::tables::TableInfo;

impl Lowerer<'_> {
    /// `node(id:)`: the row the id points to, or `null`.
    pub(crate) fn node(&self, field: &SelectedField<'_>) -> Result<RootField, Error> {
        let id_path = field.arg_path("id");
        let invalid = |m: String| {
            Error::validation(format!("invalid node id: {m}")).with_path(id_path.clone())
        };
        let id = match field.arg("id") {
            Some(Json::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => return Err(invalid("missing".to_owned())),
        };
        let node = parse_node_id(&base64_decode_lenient(&id)).map_err(&invalid)?;

        // V1 ids name no source: the table must be unique across sources.
        let matching: Vec<_> = self
            .rs
            .tables
            .ordered
            .iter()
            .filter(|t| {
                t.name.schema == node.schema
                    && t.name.name == node.table
                    && super::is_node(t)
                    && self.rs.select_access(t).is_some()
            })
            .collect();
        let qualified = format!("'{}.{}'", node.schema, node.table);
        let table = match matching.as_slice() {
            [t] => *t,
            [] => return Err(invalid(format!("no such table found: {qualified}"))),
            several => {
                let sources: Vec<String> = several.iter().map(|t| t.source.to_string()).collect();
                return Err(invalid(format!(
                    "this V1 node id matches more than one table across different sources: {qualified} exists in sources [{}]",
                    sources.join(", ")
                )));
            }
        };
        let selection = field
            .by_type
            .iter()
            .find(|(ty, _)| *ty == table.type_name())
            .map(|(_, s)| s.as_slice())
            .ok_or_else(|| Error::unexpected(format!("no Node selection for {}", table.name)))?;

        let pk = table
            .primary_key
            .as_ref()
            .ok_or_else(|| Error::unexpected("node table without primary key"))?;
        if node.values.len() < pk.len() {
            let missing: Vec<String> = pk[node.values.len()..]
                .iter()
                .map(|c| format!("'{c}'"))
                .collect();
            return Err(invalid(format!(
                "primary key columns [{}] are missing",
                missing.join(", ")
            )));
        }
        if node.values.len() > pk.len() {
            let extra: Vec<String> = node.values[pk.len()..]
                .iter()
                .map(|v| v.to_json().to_string())
                .collect();
            return Err(invalid(format!(
                "unexpected column values [{}]",
                extra.join(", ")
            )));
        }
        let mut conds = Vec::with_capacity(pk.len());
        for (column, value) in pk.iter().zip(&node.values) {
            let col = &table.columns[column];
            let text = column_value(value, &col.ty).map_err(|m| {
                Error::new(
                    Code::ParseFailed,
                    format!("value of column '{column}' in node id: {m}"),
                )
                .with_path(field.path.clone())
            })?;
            conds.push(self.key_comparison(table, column, &col.ty, text));
        }

        let select = Select {
            table: table.name.clone(),
            fields: self.fields(table, selection)?,
            args: hge_ir::SelectArgs {
                where_: Some(BoolExp::And(conds)),
                ..Default::default()
            },
            permission_filter: self.permission_filter(table),
            permission_limit: self.rs.select_access(table).and_then(|a| a.limit),
            from: None,
        };
        Ok(RootField::Database(Box::new(DatabaseRootField {
            source: table.source.clone(),
            query: QueryRoot::SelectOne(select),
        })))
    }

    /// `column = value`, compared on the redacted column when it is redacted for the role.
    fn key_comparison(
        &self,
        table: &TableInfo,
        column: &str,
        ty: &ColumnType,
        text: Option<String>,
    ) -> BoolExp {
        let cmp = BoolExp::Compare {
            column: column.to_owned(),
            ty: ty.clone(),
            op: CompareOp::Eq(Value::Literal {
                text,
                ty: ty.clone(),
            }),
        };
        match self.redaction(table, column) {
            None => cmp,
            Some(condition) => BoolExp::Redacted {
                condition: Box::new(BoolExp::Rooted(Box::new(condition.clone()))),
                exp: Box::new(cmp),
            },
        }
    }

    /// `<table>_connection` on the query or subscription root.
    pub(crate) fn connection_root(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<RootField, Error> {
        Ok(RootField::Database(Box::new(DatabaseRootField {
            source: table.source.clone(),
            query: QueryRoot::Connection(Box::new(self.connection(table, field)?)),
        })))
    }

    /// `<function>_connection` on the query or subscription root.
    pub(crate) fn function_connection_root(
        &self,
        f: &FunctionInfo,
        field: &SelectedField<'_>,
    ) -> Result<RootField, Error> {
        let table = self.table(&f.source, &f.returns)?;
        let mut conn = self.connection(table, field)?;
        conn.select.from = Some(RowSource::Function(self.function_call(f, field)?));
        Ok(RootField::Database(Box::new(DatabaseRootField {
            source: f.source.clone(),
            query: QueryRoot::Connection(Box::new(conn)),
        })))
    }

    /// A connection on `table` (root field or array relationship).
    pub(crate) fn connection(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<ConnectionSelect, Error> {
        let pk = table
            .primary_key
            .as_ref()
            .ok_or_else(|| Error::unexpected("connection on a table without primary key"))?;
        let primary_key: Vec<(String, ColumnType)> = pk
            .iter()
            .map(|c| (c.clone(), table.columns[c].ty.clone()))
            .collect();
        let args_path = format!("{}.args", field.path);

        let mut args = self.select_args(table, field)?;
        let mut cursor_paths = vec![];
        let mut cursor_types = vec![];
        if !args.order_by.is_empty() {
            if let Some(Json::Array(items)) = field.arg("order_by") {
                for item in items {
                    if let Json::Object(obj) = item {
                        self.cursor_paths(table, obj, &[], &mut cursor_paths, &mut cursor_types)?;
                    }
                }
            }
            if cursor_paths.len() != args.order_by.len() {
                return Err(Error::unexpected("order_by and cursor paths do not match"));
            }
            // V2 appends the primary key columns the ordering does not mention.
            for (column, ty) in &primary_key {
                let mentioned = args.order_by.iter().any(|ob| match &ob.target {
                    OrderByTarget::Column(c) => c == column,
                    OrderByTarget::RedactedColumn { column: c, .. } => c == column,
                    _ => false,
                });
                if !mentioned {
                    args.order_by.push(OrderBy {
                        target: match self.redaction(table, column) {
                            None => OrderByTarget::Column(column.clone()),
                            Some(condition) => OrderByTarget::RedactedColumn {
                                column: column.clone(),
                                condition: Box::new(condition.clone()),
                            },
                        },
                        direction: OrderDirection::Asc,
                        nulls: NullsOrder::Last,
                    });
                    cursor_paths.push(vec![column.clone()]);
                    cursor_types.push(ty.clone());
                }
            }
        }

        let after = field.arg("after").and_then(Json::as_str);
        let before = field.arg("before").and_then(Json::as_str);
        let split = match (after, before) {
            (Some(_), Some(_)) => {
                return Err(
                    Error::validation("\"after\" and \"before\" are not allowed at once")
                        .with_path(args_path),
                );
            }
            (Some(c), None) => Some((SplitKind::After, c)),
            (None, Some(c)) => Some((SplitKind::Before, c)),
            (None, None) => None,
        };
        let split = match split {
            None => None,
            Some((kind, cursor)) => {
                let invalid = || {
                    Error::validation("the \"after\" or \"before\" cursor is invalid")
                        .with_path(args_path.clone())
                };
                let cursor: Json = serde_json::from_slice(&base64_decode_lenient(cursor))
                    .map_err(|_| invalid())?;
                let mut items = vec![];
                if args.order_by.is_empty() {
                    for (column, ty) in &primary_key {
                        let v =
                            lookup(&cursor, std::slice::from_ref(column)).ok_or_else(invalid)?;
                        items.push(SplitItem {
                            key: SplitKey::Column(column.clone()),
                            direction: OrderDirection::Asc,
                            value: cursor_value(v, ty, &args_path)?,
                        });
                    }
                } else {
                    for (i, (ob, (path, ty))) in args
                        .order_by
                        .iter()
                        .zip(cursor_paths.iter().zip(&cursor_types))
                        .enumerate()
                    {
                        let v = lookup(&cursor, path).ok_or_else(invalid)?;
                        items.push(SplitItem {
                            key: SplitKey::OrderBy(i),
                            direction: ob.direction,
                            value: cursor_value(v, ty, &args_path)?,
                        });
                    }
                }
                Some(ConnectionSplit { kind, items })
            }
        };

        let first = non_negative_int(field, "first")?;
        let last = non_negative_int(field, "last")?;
        let slice = match (first, last) {
            (Some(_), Some(_)) => {
                return Err(
                    Error::validation("\"first\" and \"last\" are not allowed at once")
                        .with_path(args_path),
                );
            }
            (Some(n), None) => Some(ConnectionSlice::First(n)),
            (None, Some(n)) => Some(ConnectionSlice::Last(n)),
            (None, None) => None,
        };

        let fields = self.connection_fields(table, &field.selection)?;
        Ok(ConnectionSelect {
            select: Select {
                table: table.name.clone(),
                fields: vec![],
                args,
                permission_filter: self.permission_filter(table),
                permission_limit: self.rs.select_access(table).and_then(|a| a.limit),
                from: None,
            },
            primary_key,
            cursor_paths,
            split,
            slice,
            fields,
        })
    }

    fn connection_fields(
        &self,
        table: &TableInfo,
        selection: &[SelectedField<'_>],
    ) -> Result<Vec<(String, ConnectionField)>, Error> {
        let part = |sel: &SelectedField<'_>| match sel.kind {
            Selected::Field(def) => match &def.kind {
                FieldKind::ConnectionPart(p) => Some(*p),
                _ => None,
            },
            Selected::Typename(_) => None,
        };
        let mut out = vec![];
        for sel in selection {
            let f = match (sel.kind, part(sel)) {
                (Selected::Typename(t), _) => ConnectionField::Typename(t.to_owned()),
                (_, Some(ConnectionPart::PageInfo)) => {
                    let mut info = vec![];
                    for s in &sel.selection {
                        let f = match (s.kind, part(s)) {
                            (Selected::Typename(t), _) => PageInfoField::Typename(t.to_owned()),
                            (_, Some(ConnectionPart::StartCursor)) => PageInfoField::StartCursor,
                            (_, Some(ConnectionPart::EndCursor)) => PageInfoField::EndCursor,
                            (_, Some(ConnectionPart::HasNextPage)) => PageInfoField::HasNextPage,
                            (_, Some(ConnectionPart::HasPreviousPage)) => {
                                PageInfoField::HasPreviousPage
                            }
                            _ => return Err(Error::unexpected("unexpected field in PageInfo")),
                        };
                        info.push((s.alias.clone(), f));
                    }
                    ConnectionField::PageInfo(info)
                }
                (_, Some(ConnectionPart::Edges)) => {
                    let mut edges = vec![];
                    for s in &sel.selection {
                        let f = match (s.kind, part(s)) {
                            (Selected::Typename(t), _) => EdgeField::Typename(t.to_owned()),
                            (_, Some(ConnectionPart::Cursor)) => EdgeField::Cursor,
                            (_, Some(ConnectionPart::EdgeNode)) => {
                                EdgeField::Node(self.fields(table, &s.selection)?)
                            }
                            _ => return Err(Error::unexpected("unexpected field in an edge")),
                        };
                        edges.push((s.alias.clone(), f));
                    }
                    ConnectionField::Edges(edges)
                }
                _ => return Err(Error::unexpected("unexpected field in a connection")),
            };
            out.push((sel.alias.clone(), f));
        }
        Ok(out)
    }

    /// The table's node id field.
    pub(crate) fn node_id_field(table: &TableInfo) -> NodeIdField {
        NodeIdField {
            schema: table.name.schema.clone(),
            table: table.name.name.clone(),
            primary_key: table
                .primary_key
                .iter()
                .flatten()
                .map(|c| (c.clone(), table.columns[c].ty.clone()))
                .collect(),
        }
    }

    /// The cursor key path and value type of each item [`Lowerer::order_by`] makes of
    /// `obj`, in the same order (database names, as V2's cursors use them).
    fn cursor_paths(
        &self,
        table: &TableInfo,
        obj: &Map<String, Json>,
        prefix: &[String],
        paths: &mut Vec<Vec<String>>,
        types: &mut Vec<ColumnType>,
    ) -> Result<(), Error> {
        let path = |key: &str| {
            let mut p = prefix.to_vec();
            p.push(key.to_owned());
            p
        };
        for (key, value) in obj {
            if value.is_null() {
                continue;
            }
            if let Some(col) = table.column_by_gql_name(key) {
                paths.push(path(&col.name));
                types.push(col.ty.clone());
            } else if let Some(cf) = table.computed_field(key)
                && let Some(ty) = cf.scalar_type()
            {
                paths.push(path(&cf.name));
                types.push(ty.clone());
            } else if let Some(rel) = table.relationship_by_name(key) {
                let target = self.target(rel)?;
                if let Json::Object(o) = value {
                    self.cursor_paths(target, o, &path(&rel.name), paths, types)?;
                }
            } else if let Some(rel) = aggregate_relationship(table, key) {
                let target = self.target(rel)?;
                let agg = path(&format!("{}_aggregate", rel.name));
                let Json::Object(o) = value else { continue };
                for (function, v) in o {
                    if v.is_null() {
                        continue;
                    }
                    if function == "count" {
                        let mut p = agg.clone();
                        p.push("count".to_owned());
                        paths.push(p);
                        types.push(ColumnType::builtin("int8"));
                        continue;
                    }
                    let Json::Object(columns) = v else { continue };
                    for (name, order) in columns {
                        if order.is_null() {
                            continue;
                        }
                        let Some(col) = target.column_by_gql_name(name) else {
                            continue;
                        };
                        let mut p = agg.clone();
                        p.push(function.clone());
                        p.push(col.name.clone());
                        paths.push(p);
                        types.push(match function.as_str() {
                            "max" | "min" => col.ty.clone(),
                            _ => ColumnType::builtin("numeric"),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// The value at `path` in a cursor object.
fn lookup<'j>(cursor: &'j Json, path: &[String]) -> Option<&'j Json> {
    let mut v = cursor;
    for key in path {
        v = v.as_object()?.get(key)?;
    }
    Some(v)
}

fn cursor_value(v: &Json, ty: &ColumnType, path: &str) -> Result<Value, Error> {
    let text = column_value(&Scalar::from_json(v), ty)
        .map_err(|m| Error::validation(m).with_path(path.to_owned()))?;
    Ok(Value::Literal {
        text,
        ty: ty.clone(),
    })
}

/// `first` / `last`: a non-negative 32-bit integer.
fn non_negative_int(field: &SelectedField<'_>, name: &str) -> Result<Option<u64>, Error> {
    let path = field.arg_path(name);
    match field.arg(name) {
        Some(Json::Number(n)) if n.as_i64().is_some_and(|i| i < 0) => Err(Error::validation(
            "expected a non-negative 32-bit integer for type 'Int', but found an integer",
        )
        .with_path(path)),
        v => non_negative(v, &path),
    }
}
