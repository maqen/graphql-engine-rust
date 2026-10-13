//! Legacy `select`: `{"table", "columns", "where"?, "order_by"?, "limit"?, "offset"?}`.

use hge_core::{Code, QualifiedTable, Session};
use hge_ir::{
    ColumnField, Field, NullsOrder, OrderBy, OrderByTarget, OrderDirection, RelationshipSelect,
    Select, SelectArgs, SelectPermission,
};
use hge_schema::TableInfo;
use hge_schema::tables::RelKind;
use serde_json::Value as Json;

use super::boolexp::nullify_missing_in_select;
use super::json::{self, Obj, Path, err400, parse_failed};
use super::lower::Lowerer;
use hge_metadata_api::ApiError;

const SELECT_RECORD: &str = "Hasura.RQL.DML.Types.SelectG(SelectG)";

#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub source: String,
    pub table: QualifiedTable,
    pub body: SelectBody,
    pub path: Path,
}

/// The part of a select shared with nested relationship selects.
#[derive(Debug, Clone)]
pub struct SelectBody {
    columns: Vec<SelCol>,
    where_: Option<Json>,
    order_by: Option<Vec<OrderByItem>>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Clone)]
enum SelCol {
    /// `*` (depth 0), `*.*` (depth 1), …: all columns, plus relationships to that depth.
    Star(usize),
    Column(String),
    Relationship {
        name: String,
        alias: Option<String>,
        body: Box<SelectBody>,
    },
}

#[derive(Debug, Clone)]
struct OrderByItem {
    /// `col`, or `rel.….col` through object relationships.
    column: Vec<String>,
    direction: Option<OrderDirection>,
    nulls: Option<NullsOrder>,
}

impl SelectQuery {
    pub fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::new(v, path.clone(), "query")?;
        let source = o
            .string("source")?
            .unwrap_or_else(|| super::lower::DEFAULT_SOURCE.to_owned());
        let table = json::table_name(o.req("table")?, &o.at("table"))?;
        let body = SelectBody::parse(v, path)?;
        Ok(SelectQuery {
            source,
            table,
            body,
            path: path.clone(),
        })
    }
}

impl SelectBody {
    fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        let o = Obj::record(v, path.clone(), SELECT_RECORD)?;
        let columns_path = o.at("columns");
        let columns = match o.req("columns")? {
            Json::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, c)| SelCol::parse(c, &columns_path.index(i)))
                .collect::<Result<_, _>>()?,
            other => {
                return Err(json::type_mismatch("[]", "Array", other, &columns_path));
            }
        };
        let order_by = o
            .opt("order_by")
            .map(|v| parse_order_by(v, &o.at("order_by")))
            .transpose()?;
        Ok(SelectBody {
            columns,
            where_: o.opt("where").cloned(),
            order_by,
            limit: o.int("limit")?,
            offset: o.int("offset")?,
        })
    }

    fn has_args(&self) -> bool {
        self.where_.is_some()
            || self.order_by.is_some()
            || self.limit.is_some()
            || self.offset.is_some()
    }
}

impl SelCol {
    fn parse(v: &Json, path: &Path) -> Result<Self, ApiError> {
        match v {
            Json::String(s) => Ok(match wildcard_depth(s) {
                Some(depth) if depth > MAX_NESTING => return Err(too_deep(path)),
                Some(depth) => SelCol::Star(depth),
                None => SelCol::Column(s.clone()),
            }),
            Json::Object(_) => {
                let o = Obj::new(v, path.clone(), "SelCol")?;
                let name = json::string(o.req("name")?, &o.at("name"))?;
                let alias = o.string("alias")?;
                let body = SelectBody::parse(v, path)?;
                Ok(SelCol::Relationship {
                    name,
                    alias,
                    body: Box::new(body),
                })
            }
            _ => Err(parse_failed(
                "A column should either be a string or an object (relationship)",
                path,
            )),
        }
    }
}

/// Deepest relationship nesting a wildcard or order_by path may name. Lowering recurses once
/// per level, and V2 imposes no limit, so a long `*.*.…` on a self-referencing schema would
/// otherwise exhaust the stack.
const MAX_NESTING: usize = 32;

fn too_deep(path: &Path) -> ApiError {
    parse_failed(
        format!("relationships can be nested at most {MAX_NESTING} levels deep"),
        path,
    )
}

/// `*` → 0, `*.*` → 1, …; `None` for anything else.
fn wildcard_depth(s: &str) -> Option<usize> {
    let parts: Vec<&str> = s.split('.').collect();
    parts.iter().all(|p| *p == "*").then(|| parts.len() - 1)
}

fn parse_order_by(v: &Json, path: &Path) -> Result<Vec<OrderByItem>, ApiError> {
    let items = parse_order_by_items(v, path)?;
    if items.iter().any(|i| i.column.len() > MAX_NESTING + 1) {
        return Err(too_deep(path));
    }
    Ok(items)
}

fn parse_order_by_items(v: &Json, path: &Path) -> Result<Vec<OrderByItem>, ApiError> {
    match v {
        Json::String(s) => Ok(vec![order_by_string(s, path)?]),
        Json::Object(_) => Ok(vec![order_by_object(v, path)?]),
        Json::Array(items) => items
            .iter()
            .map(|item| match item {
                Json::String(s) => order_by_string(s, path),
                Json::Object(_) => order_by_object(item, path),
                _ => Err(parse_failed(
                    "expecting an object or string for order by",
                    path,
                )),
            })
            .collect(),
        _ => Err(parse_failed("Expecting : array/string/object", path)),
    }
}

fn order_by_string(s: &str, path: &Path) -> Result<OrderByItem, ApiError> {
    let (direction, rest) = match s.as_bytes().first() {
        Some(b'+') => (Some(OrderDirection::Asc), &s[1..]),
        Some(b'-') => (Some(OrderDirection::Desc), &s[1..]),
        _ => (None, s),
    };
    let column = order_by_column(rest).ok_or_else(|| {
        parse_failed(
            "string format for 'order_by' entry : {+/-}column Eg : +posted",
            path,
        )
    })?;
    Ok(OrderByItem {
        column,
        direction,
        nulls: None,
    })
}

fn order_by_column(s: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = s.split('.').map(str::to_owned).collect();
    parts.iter().all(|p| !p.is_empty()).then_some(parts)
}

fn order_by_object(v: &Json, path: &Path) -> Result<OrderByItem, ApiError> {
    let o = Obj::new(v, path.clone(), "OrderByItemG")?;
    let direction = match o.opt("type") {
        None => None,
        Some(Json::String(s)) if s == "asc" => Some(OrderDirection::Asc),
        Some(Json::String(s)) if s == "desc" => Some(OrderDirection::Desc),
        Some(_) => {
            return Err(parse_failed(
                "order type can only be 'asc' or 'desc'",
                &o.at("type"),
            ));
        }
    };
    let nulls = match o.opt("nulls") {
        None => None,
        Some(Json::String(s)) if s == "first" => Some(NullsOrder::First),
        Some(Json::String(s)) if s == "last" => Some(NullsOrder::Last),
        Some(_) => {
            return Err(parse_failed(
                "nulls can only be 'first' or 'last'",
                &o.at("nulls"),
            ));
        }
    };
    let col_path = o.at("column");
    let column = json::string(o.req("column")?, &col_path)?;
    let column = order_by_column(&column)
        .ok_or_else(|| parse_failed("column name cannot be empty", &col_path))?;
    Ok(OrderByItem {
        column,
        direction,
        nulls,
    })
}

/// A selected column after wildcard expansion.
enum ExtCol<'q> {
    Column(String),
    Relationship {
        name: String,
        alias: Option<String>,
        body: std::borrow::Cow<'q, SelectBody>,
    },
}

impl Lowerer<'_> {
    pub fn select(&self, q: &SelectQuery, session: &Session) -> Result<Select, ApiError> {
        let table = self.require_table(&q.table, &q.path.key("table"))?;
        let perm = self.select_permission(table, &q.path)?;
        let columns = self.resolve_star(table, perm, &q.body, &q.path)?;
        // V2 requires the root permission's session variables as headers; anything else
        // missing (relationship permissions) reads as NULL.
        self.check_session_variables(&perm.filter, session, &q.path)?;
        let mut select = self.select_body(table, perm, &q.body, columns, &q.path)?;
        nullify_missing_in_select(&mut select, session);
        Ok(select)
    }

    /// V2's `resolveStar`: explicit columns first, then those added by the widest wildcard.
    fn resolve_star<'q>(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        body: &'q SelectBody,
        path: &Path,
    ) -> Result<Vec<ExtCol<'q>>, ApiError> {
        let columns_path = path.key("columns");
        let mut out = vec![];
        for (i, col) in body.columns.iter().enumerate() {
            match col {
                SelCol::Star(_) => {}
                SelCol::Column(c) => out.push(ExtCol::Column(c.clone())),
                SelCol::Relationship { name, alias, body } => {
                    let rel_path = columns_path.index(i);
                    let rel = table.relationship_by_name(name).ok_or_else(|| {
                        let message = if table.columns.contains_key(name.as_str()) {
                            format!(
                                "expecting a relationship; but, \"{name}\" is a postgres column; only relationships can be expanded"
                            )
                        } else {
                            format!("relationship \"{name}\" does not exist")
                        };
                        let code = if table.columns.contains_key(name.as_str()) {
                            Code::UnexpectedPayload
                        } else {
                            Code::NotExists
                        };
                        err400(code, message, &rel_path.key("name"))
                    })?;
                    let target = self.rel_target(rel)?;
                    self.relationship_permission(rel, target, &rel_path)?;
                    out.push(ExtCol::Relationship {
                        name: name.clone(),
                        alias: alias.clone(),
                        body: std::borrow::Cow::Borrowed(body),
                    });
                }
            }
        }
        let depth = body
            .columns
            .iter()
            .filter_map(|c| match c {
                SelCol::Star(d) => Some(*d),
                _ => None,
            })
            .max();
        if let Some(depth) = depth {
            for col in self.wildcard(table, perm, depth)? {
                let dup = out.iter().any(|existing| match (existing, &col) {
                    (ExtCol::Column(a), ExtCol::Column(b)) => a == b,
                    (
                        ExtCol::Relationship { name: a, .. },
                        ExtCol::Relationship { name: b, .. },
                    ) => a == b,
                    _ => false,
                });
                if !dup {
                    out.push(col);
                }
            }
        }
        Ok(out)
    }

    /// The columns a wildcard stands for: every selectable column and, below depth 0, every
    /// relationship whose target the role can select.
    fn wildcard(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        depth: usize,
    ) -> Result<Vec<ExtCol<'static>>, ApiError> {
        let mut out: Vec<ExtCol<'static>> = table
            .columns
            .keys()
            .filter(|c| perm.allows_column(c))
            .map(|c| ExtCol::Column(c.clone()))
            .collect();
        if depth == 0 {
            return Ok(out);
        }
        for rel in table
            .object_relationships
            .iter()
            .chain(table.array_relationships.iter())
        {
            let target = self.rel_target(rel)?;
            let Some(target_perm) = hge_schema::permissions::table_permissions(target, self.role)
                .and_then(|p| p.select.as_ref())
            else {
                continue;
            };
            let columns = self
                .wildcard(target, target_perm, depth - 1)?
                .into_iter()
                .map(|c| match c {
                    ExtCol::Column(c) => SelCol::Column(c),
                    ExtCol::Relationship { name, alias, body } => SelCol::Relationship {
                        name,
                        alias,
                        body: Box::new(body.into_owned()),
                    },
                })
                .collect();
            out.push(ExtCol::Relationship {
                name: rel.name.clone(),
                alias: None,
                body: std::borrow::Cow::Owned(SelectBody {
                    columns,
                    where_: None,
                    order_by: None,
                    limit: None,
                    offset: None,
                }),
            });
        }
        Ok(out)
    }

    /// V2's `convSelectQ`: where, columns, order_by, limit, offset, in that order.
    fn select_body(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        body: &SelectBody,
        columns: Vec<ExtCol<'_>>,
        path: &Path,
    ) -> Result<Select, ApiError> {
        let where_ = body
            .where_
            .as_ref()
            .map(|w| self.bool_exp(table, perm, w, &path.key("where")))
            .transpose()?;

        let columns_path = path.key("columns");
        let mut fields = Vec::with_capacity(columns.len());
        for (i, col) in columns.into_iter().enumerate() {
            let col_path = columns_path.index(i);
            match col {
                ExtCol::Column(c) => {
                    self.check_select_column(perm, &c, &col_path)?;
                    // Unreachable while permissions name only real columns (the check above
                    // fails first, as in V2); kept as V2's messages for its own fallback.
                    let info = table.columns.get(&c).ok_or_else(|| {
                        let msg = if table.relationship_by_name(&c).is_some() {
                            format!(
                                "expecting a database column; but, \"{c}\" is a relationship; relationships have to be expanded"
                            )
                        } else {
                            format!("column \"{c}\" does not exist")
                        };
                        err400(Code::UnexpectedPayload, msg, &col_path)
                    })?;
                    fields.push((
                        c.clone(),
                        Field::Column(ColumnField {
                            redact: perm.redaction(&c).cloned().map(Box::new),
                            column: c,
                            ty: info.ty.clone(),
                            stringify: self.stringify(&info.ty),
                            json_path: None,
                        }),
                    ));
                }
                ExtCol::Relationship { name, alias, body } => {
                    let rel = table
                        .relationship_by_name(&name)
                        .expect("checked in resolve_star");
                    let target = self.rel_target(rel)?;
                    let target_perm = self.relationship_permission(rel, target, &col_path)?;
                    let nested_columns =
                        self.resolve_star(target, target_perm, &body, &col_path)?;
                    let select =
                        self.select_body(target, target_perm, &body, nested_columns, &col_path)?;
                    let key = alias.unwrap_or(name);
                    let rel_select = Box::new(RelationshipSelect {
                        mapping: rel.mapping.clone(),
                        select,
                    });
                    fields.push((
                        key,
                        match rel.kind {
                            RelKind::Object => {
                                if body.has_args() {
                                    return Err(err400(
                                        Code::UnexpectedPayload,
                                        "when selecting an 'obj_relationship' 'where', 'order_by', 'limit' and 'offset'  can't be used",
                                        &col_path,
                                    ));
                                }
                                Field::ObjectRelationship(rel_select)
                            }
                            RelKind::Array => Field::ArrayRelationship(rel_select),
                        },
                    ));
                }
            }
        }

        let mut order_by = vec![];
        if let Some(items) = &body.order_by {
            let ob_path = path.key("order_by");
            for (i, item) in items.iter().enumerate() {
                let target = self.order_by_target(table, perm, &item.column, &ob_path.index(i))?;
                let direction = item.direction.unwrap_or(OrderDirection::Asc);
                // Without `nulls`, V2 emits no NULLS clause; the explicit Postgres default for
                // the direction sorts the same.
                let nulls = item.nulls.unwrap_or(match direction {
                    OrderDirection::Asc => NullsOrder::Last,
                    OrderDirection::Desc => NullsOrder::First,
                });
                order_by.push(OrderBy {
                    target,
                    direction,
                    nulls,
                });
            }
        }

        let limit = non_negative(body.limit, &path.key("limit"))?;
        let offset = non_negative(body.offset, &path.key("offset"))?;

        Ok(Select {
            table: table.name.clone(),
            fields,
            args: SelectArgs {
                where_,
                order_by,
                limit,
                offset,
                distinct_on: vec![],
            },
            permission_filter: perm.row_filter().cloned(),
            permission_limit: perm.limit,
        })
    }

    /// V2's `convOrderByElem`.
    fn order_by_target(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        column: &[String],
        path: &Path,
    ) -> Result<OrderByTarget, ApiError> {
        let (name, rest) = column.split_first().expect("non-empty order_by column");
        let not_exists = || err400(Code::NotExists, format!("\"{name}\" does not exist"), path);
        if rest.is_empty() {
            if let Some(col) = table.columns.get(name.as_str()) {
                self.check_select_column(perm, name, path)?;
                if hge_schema::scalars::is_geo(&col.ty) {
                    return Err(err400(
                        Code::UnexpectedPayload,
                        format!("\"{name}\" has type 'geometry' and cannot be used in order_by"),
                        path,
                    ));
                }
                // Inherited roles: a redacted column sorts as NULL where it is hidden.
                return Ok(match perm.redaction(name) {
                    None => OrderByTarget::Column(name.clone()),
                    Some(condition) => OrderByTarget::RedactedColumn {
                        column: name.clone(),
                        condition: Box::new(condition.clone()),
                    },
                });
            }
            if table.relationship_by_name(name).is_some() {
                return Err(err400(
                    Code::UnexpectedPayload,
                    format!("\"{name}\" is a relationship and should be expanded"),
                    path,
                ));
            }
            return Err(not_exists());
        }
        if table.columns.contains_key(name.as_str()) {
            return Err(err400(
                Code::UnexpectedPayload,
                format!("\"{name}\" is a Postgres column and cannot be chained further"),
                path,
            ));
        }
        let rel = table.relationship_by_name(name).ok_or_else(not_exists)?;
        if rel.kind == RelKind::Array {
            return Err(err400(
                Code::UnexpectedPayload,
                format!("\"{name}\" is an array relationship and can't be used in 'order_by'"),
                path,
            ));
        }
        let target = self.rel_target(rel)?;
        let target_perm = self.relationship_permission(rel, target, path)?;
        let inner = self.order_by_target(target, target_perm, rest, path)?;
        Ok(OrderByTarget::ObjectRelationship {
            table: target.name.clone(),
            mapping: rel.mapping.clone(),
            filter: target_perm.row_filter().cloned().map(Box::new),
            target: Box::new(inner),
        })
    }
}

/// V2's `onlyPositiveInt`.
fn non_negative(v: Option<i64>, path: &Path) -> Result<Option<u64>, ApiError> {
    match v {
        Some(n) if n < 0 => Err(err400(
            Code::NotSupported,
            "unexpected negative value",
            path,
        )),
        Some(n) => Ok(Some(n as u64)),
        None => Ok(None),
    }
}
