//! Lowering a normalised operation to IR.

use std::sync::Arc;

use hge_core::{ColumnType, Error};
use hge_ir::{
    BoolExp, ColumnField, CompareOp, DatabaseRootField, Field, JsonPathElement, NullsOrder,
    Operation, OrderBy, OrderByTarget, OrderDirection, QueryRoot, RelationshipSelect, RootField,
    Select, SelectArgs, Value,
};
use serde_json::{Map, Value as Json};

use crate::build::RoleSchema;
use crate::introspection;
use crate::model::FieldKind;
use crate::normalize::{NormalizedOperation, Selected, SelectedField};
use crate::scalars;
use crate::tables::{RelationshipInfo, TableInfo};

const NO_QUERIES_MESSAGE: &str = "There are no queries available to the current role. Either there are no sources or remote schemas configured, or the current role doesn't have the required permissions.";

pub fn lower(rs: &RoleSchema, op: &NormalizedOperation<'_>) -> Result<Operation, Error> {
    let lw = Lowerer { rs };
    let mut root_fields = Vec::with_capacity(op.fields.len());
    for field in &op.fields {
        let rf = match field.kind {
            Selected::Typename(t) => RootField::Static(Json::String(t.to_owned())),
            Selected::Field(def) => match &def.kind {
                FieldKind::SchemaField | FieldKind::TypeField => {
                    RootField::Static(introspection::resolve_root(&rs.schema, field))
                }
                FieldKind::NoQueriesAvailable => {
                    RootField::Static(Json::String(NO_QUERIES_MESSAGE.to_owned()))
                }
                FieldKind::SelectMany(table) => RootField::Database(Box::new(DatabaseRootField {
                    source: table.source.clone(),
                    query: QueryRoot::Select(lw.select(table, field, true)?),
                })),
                FieldKind::SelectByPk(table) => RootField::Database(Box::new(DatabaseRootField {
                    source: table.source.clone(),
                    query: QueryRoot::SelectOne(lw.select_by_pk(table, field)?),
                })),
                other => {
                    return Err(Error::unexpected(format!(
                        "unexpected root field kind {other:?}"
                    )));
                }
            },
        };
        root_fields.push((field.alias.clone(), rf));
    }
    Ok(Operation {
        kind: op.kind,
        root_fields,
    })
}

struct Lowerer<'a> {
    rs: &'a RoleSchema,
}

impl Lowerer<'_> {
    fn target(&self, rel: &RelationshipInfo) -> Result<&Arc<TableInfo>, Error> {
        self.rs
            .tables
            .get(&rel.source, &rel.target)
            .ok_or_else(|| Error::unexpected(format!("unknown table {}", rel.target)))
    }

    /// The role's select filter on `table`, unless it lets every row through.
    fn permission_filter(&self, table: &TableInfo) -> Option<BoolExp> {
        self.rs
            .select_access(table)
            .and_then(|a| a.row_filter())
            .cloned()
    }

    fn select(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
        with_args: bool,
    ) -> Result<Select, Error> {
        let fields = self.fields(table, &field.selection)?;
        let args = if with_args {
            self.select_args(table, field)?
        } else {
            SelectArgs::default()
        };
        Ok(Select {
            table: table.name.clone(),
            fields,
            args,
            permission_filter: self.permission_filter(table),
            permission_limit: self.rs.select_access(table).and_then(|a| a.limit),
        })
    }

    fn select_by_pk(&self, table: &TableInfo, field: &SelectedField<'_>) -> Result<Select, Error> {
        let mut select = self.select(table, field, false)?;
        let pk = table
            .primary_key
            .as_ref()
            .ok_or_else(|| Error::unexpected("by_pk field on table without primary key"))?;
        let mut conds = Vec::with_capacity(pk.len());
        for col in pk {
            let col = &table.columns[col];
            let value = field.arg(&col.gql_name).ok_or_else(|| {
                Error::validation(format!("missing required field '{}'", col.gql_name))
                    .with_path(field.arg_path(&col.gql_name))
            })?;
            conds.push(BoolExp::Compare {
                column: col.name.clone(),
                ty: col.ty.clone(),
                op: CompareOp::Eq(to_value(value, &col.ty, &field.arg_path(&col.gql_name))?),
            });
        }
        select.args.where_ = Some(BoolExp::And(conds));
        Ok(select)
    }

    fn fields(
        &self,
        _table: &TableInfo,
        selection: &[SelectedField<'_>],
    ) -> Result<Vec<(String, Field)>, Error> {
        let mut out = Vec::with_capacity(selection.len());
        for sel in selection {
            let f = match sel.kind {
                Selected::Typename(t) => Field::Typename(t.to_owned()),
                Selected::Field(def) => match &def.kind {
                    FieldKind::Column { column, ty } => {
                        let json_path = match sel.arg("path") {
                            Some(Json::String(p)) => Some(parse_json_path(p).map_err(|e| {
                                Error::validation(e).with_path(sel.arg_path("path"))
                            })?),
                            _ => None,
                        };
                        Field::Column(ColumnField {
                            column: column.clone(),
                            ty: ty.clone(),
                            stringify: self.rs.options.stringify_numeric_types
                                && scalars::is_big_num(ty),
                            json_path,
                        })
                    }
                    FieldKind::ObjectRelationship(rel) => {
                        let target = self.target(rel)?;
                        Field::ObjectRelationship(Box::new(RelationshipSelect {
                            mapping: rel.mapping.clone(),
                            select: self.select(target, sel, false)?,
                        }))
                    }
                    FieldKind::ArrayRelationship(rel) => {
                        let target = self.target(rel)?;
                        Field::ArrayRelationship(Box::new(RelationshipSelect {
                            mapping: rel.mapping.clone(),
                            select: self.select(target, sel, true)?,
                        }))
                    }
                    other => {
                        return Err(Error::unexpected(format!(
                            "unexpected field kind {other:?} in table selection"
                        )));
                    }
                },
            };
            out.push((sel.alias.clone(), f));
        }
        Ok(out)
    }

    fn select_args(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<SelectArgs, Error> {
        let mut args = SelectArgs::default();
        if let Some(w) = field.arg("where")
            && let Json::Object(obj) = w
        {
            args.where_ = self.bool_exp(table, obj, &field.arg_path("where"))?;
        }
        if let Some(Json::Array(items)) = field.arg("order_by") {
            let path = field.arg_path("order_by");
            for (i, item) in items.iter().enumerate() {
                if let Json::Object(obj) = item {
                    self.order_by(table, obj, &format!("{path}[{i}]"), &mut args.order_by)?;
                }
            }
        }
        args.limit = non_negative(field.arg("limit"), &field.arg_path("limit"))?;
        args.offset = non_negative(field.arg("offset"), &field.arg_path("offset"))?;
        if let Some(Json::Array(cols)) = field.arg("distinct_on") {
            for c in cols {
                let name = c.as_str().unwrap_or_default();
                let col = table.column_by_gql_name(name).ok_or_else(|| {
                    Error::validation(format!("unknown column '{name}'"))
                        .with_path(field.arg_path("distinct_on"))
                })?;
                args.distinct_on.push(col.name.clone());
            }
            if !args.distinct_on.is_empty() && !args.order_by.is_empty() {
                let prefix_ok = args.order_by.len() >= args.distinct_on.len()
                    && args
                        .distinct_on
                        .iter()
                        .zip(&args.order_by)
                        .all(|(d, o)| matches!(&o.target, OrderByTarget::Column(c) if c == d));
                if !prefix_ok {
                    return Err(Error::validation(
                        "\"distinct_on\" columns must match initial \"order_by\" columns",
                    )
                    .with_path(field.arg_path("distinct_on")));
                }
            }
        }
        Ok(args)
    }

    fn bool_exp(
        &self,
        table: &TableInfo,
        obj: &Map<String, Json>,
        path: &str,
    ) -> Result<Option<BoolExp>, Error> {
        let mut items = Vec::with_capacity(obj.len());
        for (key, value) in obj {
            if value.is_null() {
                continue;
            }
            let item_path = format!("{path}.{key}");
            match key.as_str() {
                "_and" | "_or" => {
                    let mut parts = vec![];
                    for (i, v) in as_list(value).iter().enumerate() {
                        if let Json::Object(o) = v {
                            parts.push(
                                self.bool_exp(table, o, &format!("{item_path}[{i}]"))?
                                    .unwrap_or_else(BoolExp::always_true),
                            );
                        }
                    }
                    items.push(if key == "_and" {
                        BoolExp::And(parts)
                    } else {
                        BoolExp::Or(parts)
                    });
                }
                "_not" => {
                    if let Json::Object(o) = value {
                        let inner = self
                            .bool_exp(table, o, &item_path)?
                            .unwrap_or_else(BoolExp::always_true);
                        items.push(BoolExp::Not(Box::new(inner)));
                    }
                }
                _ => {
                    if let Some(col) = table.column_by_gql_name(key) {
                        if let Json::Object(ops) = value {
                            self.comparisons(&col.name, &col.ty, ops, &item_path, &mut items)?;
                        }
                    } else if let Some(rel) = table.relationship_by_name(key) {
                        let target = self.target(rel)?;
                        if let Json::Object(o) = value {
                            let user = self.bool_exp(target, o, &item_path)?;
                            // Filtering through a relationship never sees rows the role
                            // cannot select.
                            let permission = self
                                .permission_filter(target)
                                .map(|p| BoolExp::Rooted(Box::new(p)));
                            let exp =
                                BoolExp::and(permission, user).unwrap_or_else(BoolExp::always_true);
                            items.push(BoolExp::Relationship {
                                table: target.name.clone(),
                                mapping: rel.mapping.clone(),
                                exp: Box::new(exp),
                            });
                        }
                    } else {
                        return Err(Error::validation(format!(
                            "field '{key}' not found in type: '{}'",
                            table.bool_exp_name()
                        ))
                        .with_path(item_path));
                    }
                }
            }
        }
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(BoolExp::And(items)),
        })
    }

    fn comparisons(
        &self,
        column: &str,
        ty: &ColumnType,
        ops: &Map<String, Json>,
        path: &str,
        out: &mut Vec<BoolExp>,
    ) -> Result<(), Error> {
        for (op, v) in ops {
            let op_path = format!("{path}.{op}");
            if v.is_null() {
                if self.rs.options.dangerously_collapse_booleans || op == "_is_null" {
                    continue;
                }
                return Err(Error::validation(format!(
                    "unexpected null value for type '{}'",
                    scalars::scalar_name(&ty.element())
                ))
                .with_path(op_path));
            }
            let val = |t: &ColumnType| to_value(v, t, &op_path);
            let text_ty = ColumnType::builtin("text");
            let cmp = match op.as_str() {
                "_eq" => CompareOp::Eq(val(ty)?),
                "_neq" => CompareOp::Neq(val(ty)?),
                "_gt" => CompareOp::Gt(val(ty)?),
                "_lt" => CompareOp::Lt(val(ty)?),
                "_gte" => CompareOp::Gte(val(ty)?),
                "_lte" => CompareOp::Lte(val(ty)?),
                "_in" => CompareOp::In(val(&ty.array_of())?),
                "_nin" => CompareOp::Nin(val(&ty.array_of())?),
                "_like" => CompareOp::Like(val(ty)?),
                "_nlike" => CompareOp::NLike(val(ty)?),
                "_ilike" => CompareOp::ILike(val(ty)?),
                "_nilike" => CompareOp::NILike(val(ty)?),
                "_similar" => CompareOp::Similar(val(ty)?),
                "_nsimilar" => CompareOp::NSimilar(val(ty)?),
                "_regex" => CompareOp::Regex(val(ty)?),
                "_nregex" => CompareOp::NRegex(val(ty)?),
                "_iregex" => CompareOp::IRegex(val(ty)?),
                "_niregex" => CompareOp::NIRegex(val(ty)?),
                "_is_null" => CompareOp::IsNull(v.as_bool().unwrap_or(false)),
                "_contains" => CompareOp::Contains(val(ty)?),
                "_contained_in" => CompareOp::ContainedIn(val(ty)?),
                "_has_key" => CompareOp::HasKey(val(&text_ty)?),
                "_has_keys_any" => CompareOp::HasKeysAny(val(&text_ty.array_of())?),
                "_has_keys_all" => CompareOp::HasKeysAll(val(&text_ty.array_of())?),
                other => {
                    return Err(Error::new(
                        hge_core::Code::NotSupported,
                        format!("the operator '{other}' is not supported yet"),
                    )
                    .with_path(op_path));
                }
            };
            out.push(BoolExp::Compare {
                column: column.to_owned(),
                ty: ty.clone(),
                op: cmp,
            });
        }
        Ok(())
    }

    fn order_by(
        &self,
        table: &TableInfo,
        obj: &Map<String, Json>,
        path: &str,
        out: &mut Vec<OrderBy>,
    ) -> Result<(), Error> {
        for (key, value) in obj {
            if value.is_null() {
                continue;
            }
            let item_path = format!("{path}.{key}");
            if let Some(col) = table.column_by_gql_name(key) {
                let (direction, nulls) = order_type(value.as_str().unwrap_or("asc"));
                out.push(OrderBy {
                    target: OrderByTarget::Column(col.name.clone()),
                    direction,
                    nulls,
                });
            } else if let Some(rel) = table.relationship_by_name(key) {
                let target = self.target(rel)?;
                if let Json::Object(o) = value {
                    let mut nested = vec![];
                    self.order_by(target, o, &item_path, &mut nested)?;
                    for item in nested {
                        out.push(OrderBy {
                            target: OrderByTarget::ObjectRelationship {
                                table: target.name.clone(),
                                mapping: rel.mapping.clone(),
                                filter: self.permission_filter(target).map(Box::new),
                                target: Box::new(item.target),
                            },
                            direction: item.direction,
                            nulls: item.nulls,
                        });
                    }
                }
            } else {
                return Err(Error::validation(format!(
                    "field '{key}' not found in type: '{}'",
                    table.order_by_name()
                ))
                .with_path(item_path));
            }
        }
        Ok(())
    }
}

fn order_type(s: &str) -> (OrderDirection, NullsOrder) {
    match s {
        "asc_nulls_first" => (OrderDirection::Asc, NullsOrder::First),
        "desc" | "desc_nulls_first" => (OrderDirection::Desc, NullsOrder::First),
        "desc_nulls_last" => (OrderDirection::Desc, NullsOrder::Last),
        _ => (OrderDirection::Asc, NullsOrder::Last),
    }
}

fn as_list(v: &Json) -> Vec<Json> {
    match v {
        Json::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

fn non_negative(v: Option<&Json>, path: &str) -> Result<Option<u64>, Error> {
    match v {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => {
            match n.as_i64() {
                Some(i) if i >= 0 => Ok(Some(i as u64)),
                _ => Err(Error::validation("expecting a non-negative integer")
                    .with_path(path.to_owned())),
            }
        }
        Some(_) => {
            Err(Error::validation("expecting a non-negative integer").with_path(path.to_owned()))
        }
    }
}

/// Convert a coerced JSON input value to a typed IR value (text representation).
pub fn to_value(v: &Json, ty: &ColumnType, path: &str) -> Result<Value, Error> {
    Ok(Value::Literal {
        text: to_text(v, ty, path)?,
        ty: ty.clone(),
    })
}

fn to_text(v: &Json, ty: &ColumnType, path: &str) -> Result<Option<String>, Error> {
    if v.is_null() {
        return Ok(None);
    }
    if ty.is_array() {
        return match v {
            Json::Array(items) => {
                let elem = ColumnType {
                    dimensions: ty.dimensions - 1,
                    ..ty.clone()
                };
                let mut out = String::from("{");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    match to_text(item, &elem, path)? {
                        None => out.push_str("NULL"),
                        Some(t) if elem.is_array() => out.push_str(&t),
                        Some(t) => {
                            out.push('"');
                            for c in t.chars() {
                                if c == '"' || c == '\\' {
                                    out.push('\\');
                                }
                                out.push(c);
                            }
                            out.push('"');
                        }
                    }
                }
                out.push('}');
                Ok(Some(out))
            }
            // A string is passed through as a Postgres array literal.
            Json::String(s) => Ok(Some(s.clone())),
            _ => Err(Error::validation(format!(
                "expected a list for an array of type '{}'",
                scalars::scalar_name(&ty.element())
            ))
            .with_path(path.to_owned())),
        };
    }
    if scalars::is_json_like(ty) {
        return Ok(Some(v.to_string()));
    }
    Ok(Some(match v {
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => b.to_string(),
        other => other.to_string(),
    }))
}

/// Parse a V2 JSON path argument (`$.a.b[0]`, `a`, `['key']`, `objs[0]["k"]`).
pub fn parse_json_path(input: &str) -> Result<Vec<JsonPathElement>, String> {
    let s = input.strip_prefix('$').unwrap_or(input);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    let err = |msg: &str| format!("parse json path error: {msg}: {input}");
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                if i < chars.len() && chars[i] == '[' {
                    continue;
                }
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                if start == i {
                    return Err(err("empty key"));
                }
                out.push(JsonPathElement::Key(chars[start..i].iter().collect()));
            }
            '[' => {
                i += 1;
                if i < chars.len() && (chars[i] == '\'' || chars[i] == '"') {
                    let quote = chars[i];
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != quote {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(err("unterminated string"));
                    }
                    out.push(JsonPathElement::Key(chars[start..i].iter().collect()));
                    i += 1;
                } else {
                    let start = i;
                    while i < chars.len() && chars[i] != ']' {
                        i += 1;
                    }
                    let idx: String = chars[start..i].iter().collect();
                    out.push(JsonPathElement::Index(
                        idx.trim().parse().map_err(|_| err("invalid array index"))?,
                    ));
                }
                if i >= chars.len() || chars[i] != ']' {
                    return Err(err("expected ']'"));
                }
                i += 1;
            }
            _ if out.is_empty() && i == 0 => {
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                out.push(JsonPathElement::Key(chars[start..i].iter().collect()));
            }
            c => return Err(err(&format!("unexpected character '{c}'"))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use JsonPathElement::{Index, Key};

    #[test]
    fn json_paths() {
        assert_eq!(parse_json_path("$").unwrap(), vec![]);
        assert_eq!(
            parse_json_path(".obj.c1").unwrap(),
            vec![Key("obj".into()), Key("c1".into())]
        );
        assert_eq!(parse_json_path("a").unwrap(), vec![Key("a".into())]);
        assert_eq!(
            parse_json_path("arr[0]").unwrap(),
            vec![Key("arr".into()), Index(0)]
        );
        assert_eq!(
            parse_json_path("['!@#$%^']").unwrap(),
            vec![Key("!@#$%^".into())]
        );
        assert_eq!(
            parse_json_path("objs.[0].[\"你好\"]").unwrap(),
            vec![Key("objs".into()), Index(0), Key("你好".into())]
        );
        assert_eq!(
            parse_json_path("translations['hello world!']").unwrap(),
            vec![Key("translations".into()), Key("hello world!".into())]
        );
    }

    #[test]
    fn array_literals() {
        let ty = ColumnType::builtin("text").array_of();
        let v = serde_json::json!(["a", "b\"c", null, "d\\e"]);
        assert_eq!(
            to_text(&v, &ty, "$").unwrap().unwrap(),
            r#"{"a","b\"c",NULL,"d\\e"}"#
        );
    }
}
