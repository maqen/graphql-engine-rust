//! Boolean expressions in permission metadata (`filter`, `check`) and preset values (`set`).
//!
//! Unlike GraphQL `where` arguments these are untyped JSON written by the user, so this is
//! where V2's permission-only syntax lives: `$`-prefixed operators, the `{col: value}`
//! shorthand, `_exists`, column comparisons (`_ceq`, `["$", col]`), null-safe equality and
//! session variable references.

use hge_core::{ColumnType, QualifiedTable};
use hge_ir::{BoolExp, ColumnCompareOp, ColumnRef, CompareOp, FunctionCall, Value};
use serde_json::Value as Json;

use crate::functions::{ComputedFieldInfo, ComputedReturn};
use crate::scalars;
use crate::tables::{ColumnInfo, TableInfo, Tables};

/// Turn the column comparisons [`Parser::column`] produced for a computed field into
/// comparisons of its value.
fn computed_comparison(exp: BoolExp, call: &FunctionCall) -> Result<BoolExp, PermError> {
    Ok(match exp {
        BoolExp::And(items) => BoolExp::And(
            items
                .into_iter()
                .map(|e| computed_comparison(e, call))
                .collect::<Result<_, _>>()?,
        ),
        BoolExp::Compare { ty, op, .. } => BoolExp::CompareComputed {
            call: call.clone(),
            ty,
            op,
        },
        _ => {
            return Err(
                "column comparison operators are not supported on computed fields".to_owned(),
            );
        }
    })
}

/// A permission problem; becomes a metadata inconsistency.
pub type PermError = String;

/// Parses permission expressions against the resolved tables of one source.
pub struct Parser<'a> {
    pub tables: &'a Tables,
}

impl Parser<'_> {
    /// Parse a `filter` / `check` expression on `table`.
    pub fn bool_exp(&self, table: &TableInfo, json: &Json) -> Result<BoolExp, PermError> {
        self.exp(table, table, json)
    }

    /// `exp` is evaluated against `table`; `root` is the table the permission belongs to.
    fn exp(&self, table: &TableInfo, root: &TableInfo, json: &Json) -> Result<BoolExp, PermError> {
        let Json::Object(obj) = json else {
            return Err(format!(
                "expected a boolean expression object, but found {}",
                json_kind(json)
            ));
        };
        let mut items = Vec::with_capacity(obj.len());
        for (key, value) in obj {
            items.push(self.item(table, root, key, value)?);
        }
        Ok(match items.len() {
            1 => items.pop().expect("one item"),
            _ => BoolExp::And(items),
        })
    }

    fn item(
        &self,
        table: &TableInfo,
        root: &TableInfo,
        key: &str,
        value: &Json,
    ) -> Result<BoolExp, PermError> {
        match key {
            "_and" | "$and" | "_or" | "$or" => {
                let parts = match value {
                    Json::Array(items) => items
                        .iter()
                        .map(|v| self.exp(table, root, v))
                        .collect::<Result<Vec<_>, _>>()?,
                    other => vec![self.exp(table, root, other)?],
                };
                Ok(if key.ends_with("and") {
                    BoolExp::And(parts)
                } else {
                    BoolExp::Or(parts)
                })
            }
            "_not" | "$not" => Ok(BoolExp::Not(Box::new(self.exp(table, root, value)?))),
            "_exists" => self.exists(table, root, value),
            _ => {
                if let Some(col) = table.columns.get(key) {
                    return self.column(table, root, col, value);
                }
                if let Some(rel) = table.relationship_by_name(key) {
                    let target = self.tables.get(&rel.source, &rel.target).ok_or_else(|| {
                        format!(
                            "relationship \"{key}\" of table \"{}\" points at an untracked table",
                            table.display_name()
                        )
                    })?;
                    return Ok(BoolExp::Relationship {
                        table: target.name.clone(),
                        mapping: rel.mapping.clone(),
                        exp: Box::new(self.exp(target, root, value)?),
                    });
                }
                if let Some(cf) = table.computed_field(key) {
                    return self.computed_field(table, root, cf, value);
                }
                Err(format!(
                    "no such column exists: \"{key}\" in table \"{}\"",
                    table.display_name()
                ))
            }
        }
    }

    /// A computed field without input arguments, compared like a column (scalar) or
    /// filtered like a relationship (table).
    fn computed_field(
        &self,
        table: &TableInfo,
        root: &TableInfo,
        cf: &ComputedFieldInfo,
        value: &Json,
    ) -> Result<BoolExp, PermError> {
        if cf.has_input_args() {
            return Err(format!(
                "computed field \"{}\" has input arguments and cannot be used in a boolean expression",
                cf.name
            ));
        }
        let call = cf.call(&table.name, vec![]).map_err(str::to_owned)?;
        match &cf.returns {
            ComputedReturn::Scalar(ty) => {
                // Parse the operators as if the value were a column, then compare the call.
                let as_column = ColumnInfo {
                    name: cf.name.clone(),
                    gql_name: cf.name.clone(),
                    ty: ty.clone(),
                    nullable: true,
                    description: None,
                    mutable: false,
                    enum_ref: None,
                };
                let exp = self.column(table, root, &as_column, value)?;
                computed_comparison(exp, &call)
            }
            ComputedReturn::Table(target) => {
                let target = self.tables.get(&table.source, target).ok_or_else(|| {
                    format!(
                        "computed field \"{}\" of table \"{}\" returns an untracked table",
                        cf.name,
                        table.display_name()
                    )
                })?;
                Ok(BoolExp::ComputedTable {
                    call,
                    exp: Box::new(self.exp(target, root, value)?),
                })
            }
        }
    }

    fn exists(
        &self,
        table: &TableInfo,
        root: &TableInfo,
        value: &Json,
    ) -> Result<BoolExp, PermError> {
        let obj = value
            .as_object()
            .ok_or("expected an object with keys \"_table\" and \"_where\" in \"_exists\"")?;
        let name: QualifiedTable = obj
            .get("_table")
            .cloned()
            .ok_or("key \"_table\" not found in \"_exists\"")
            .and_then(|t| {
                serde_json::from_value(t).map_err(|_| "invalid table name in \"_exists\"")
            })?;
        let target = self.tables.get(&table.source, &name).ok_or_else(|| {
            format!(
                "table \"{}\" in \"_exists\" is not tracked in source \"{}\"",
                name, table.source
            )
        })?;
        let where_ = obj
            .get("_where")
            .ok_or("key \"_where\" not found in \"_exists\"")?;
        Ok(BoolExp::Exists {
            table: target.name.clone(),
            exp: Box::new(self.exp(target, root, where_)?),
        })
    }

    fn column(
        &self,
        table: &TableInfo,
        root: &TableInfo,
        col: &ColumnInfo,
        value: &Json,
    ) -> Result<BoolExp, PermError> {
        if !col.ty.is_array() && col.ty.name == "json" {
            return Err("JSON column can not be part of boolean expression".to_owned());
        }
        let Json::Object(ops) = value else {
            // `{col: value}` is always compared null-safely, whatever the nullability.
            return self.equality(col, value, true, false);
        };
        let mut items = Vec::with_capacity(ops.len());
        for (op, v) in ops {
            items.push(self.operator(table, root, col, op, v)?);
        }
        Ok(match items.len() {
            1 => items.pop().expect("one item"),
            _ => BoolExp::And(items),
        })
    }

    fn operator(
        &self,
        table: &TableInfo,
        root: &TableInfo,
        col: &ColumnInfo,
        op: &str,
        v: &Json,
    ) -> Result<BoolExp, PermError> {
        // Every operator has a `$` twin (`$eq` for `_eq`).
        if !(op.starts_with('$') || op.starts_with('_')) {
            return Err(format!("Unknown operator: {op}"));
        }
        let name = &op[1..];
        let ty = &col.ty;
        let text = ColumnType::builtin("text");
        let cmp = |op: CompareOp| {
            Ok(BoolExp::Compare {
                column: col.name.clone(),
                ty: ty.clone(),
                op,
            })
        };
        let scalar = |v: &Json| value(v, ty);
        let adapt =
            |v: &Json, ty: &ColumnType, _: &str| value(v, ty).map_err(hge_core::Error::validation);
        match name {
            "eq" => self.equality(col, v, col.nullable, false),
            "ne" | "neq" => self.equality(col, v, col.nullable, true),
            "gt" => cmp(CompareOp::Gt(scalar(v)?)),
            "lt" => cmp(CompareOp::Lt(scalar(v)?)),
            "gte" => cmp(CompareOp::Gte(scalar(v)?)),
            "lte" => cmp(CompareOp::Lte(scalar(v)?)),
            "in" => cmp(CompareOp::In(value(v, &ty.array_of())?)),
            "nin" => cmp(CompareOp::Nin(value(v, &ty.array_of())?)),
            "like" | "nlike" | "ilike" | "nilike" | "similar" | "nsimilar" | "regex" | "nregex"
            | "iregex" | "niregex" => {
                if !scalars::is_string_type(ty) {
                    return Err(format!(
                        "operator {op} is only supported on string columns, not on column \"{}\"",
                        col.name
                    ));
                }
                let v = scalar(v)?;
                cmp(match name {
                    "like" => CompareOp::Like(v),
                    "nlike" => CompareOp::NLike(v),
                    "ilike" => CompareOp::ILike(v),
                    "nilike" => CompareOp::NILike(v),
                    "similar" => CompareOp::Similar(v),
                    "nsimilar" => CompareOp::NSimilar(v),
                    "regex" => CompareOp::Regex(v),
                    "nregex" => CompareOp::NRegex(v),
                    "iregex" => CompareOp::IRegex(v),
                    _ => CompareOp::NIRegex(v),
                })
            }
            "is_null" => match v {
                Json::Bool(b) => cmp(CompareOp::IsNull(*b)),
                other => Err(format!(
                    "expected a boolean for {op}, but found {}",
                    json_kind(other)
                )),
            },
            "contains" | "contained_in" => {
                if !(ty.is_array() || scalars::is_jsonb(ty)) {
                    return Err(format!(
                        "operator {op} is only supported on jsonb and array columns"
                    ));
                }
                let v = scalar(v)?;
                cmp(if name == "contains" {
                    CompareOp::Contains(v)
                } else {
                    CompareOp::ContainedIn(v)
                })
            }
            "has_key" | "has_keys_any" | "has_keys_all" => {
                if !scalars::is_jsonb(ty) {
                    return Err(format!("operator {op} is only supported on jsonb columns"));
                }
                cmp(match name {
                    "has_key" => CompareOp::HasKey(value(v, &text)?),
                    "has_keys_any" => CompareOp::HasKeysAny(value(v, &text.array_of())?),
                    _ => CompareOp::HasKeysAll(value(v, &text.array_of())?),
                })
            }
            "jsonb_path_exists" | "jsonb_path_match" => {
                if !scalars::is_jsonb(ty) {
                    return Err(format!("operator {op} is only supported on jsonb columns"));
                }
                let v = value(v, &ColumnType::builtin("jsonpath"))?;
                cmp(if name == "jsonb_path_exists" {
                    CompareOp::JsonbPathExists(v)
                } else {
                    CompareOp::JsonbPathMatch(v)
                })
            }
            "ceq" | "cne" | "cneq" | "cgt" | "clt" | "cgte" | "clte" => {
                let op = match name {
                    "ceq" => ColumnCompareOp::Eq,
                    "cne" | "cneq" => ColumnCompareOp::Neq,
                    "cgt" => ColumnCompareOp::Gt,
                    "clt" => ColumnCompareOp::Lt,
                    "cgte" => ColumnCompareOp::Gte,
                    _ => ColumnCompareOp::Lte,
                };
                let (other_table, other) = match v {
                    Json::String(c) => (table, ColumnRef::Current(c.clone())),
                    Json::Array(items)
                        if items.len() == 2 && items[0] == "$" && items[1].is_string() =>
                    {
                        let c = items[1].as_str().expect("checked").to_owned();
                        (root, ColumnRef::Root(c))
                    }
                    _ => {
                        return Err(
                            "Relationship references are not supported in column comparison RHS"
                                .to_owned(),
                        );
                    }
                };
                let other_name = match &other {
                    ColumnRef::Current(c) | ColumnRef::Root(c) => c,
                };
                let other_col = other_table.columns.get(other_name).ok_or_else(|| {
                    format!(
                        "no such column exists: \"{other_name}\" in table \"{}\"",
                        other_table.display_name()
                    )
                })?;
                if other_col.ty != col.ty {
                    return Err(format!(
                        "incompatible column types: \"{}\" has type {}, but \"{}\" has type {}",
                        col.name,
                        scalars::scalar_name(&col.ty),
                        other_col.name,
                        scalars::scalar_name(&other_col.ty)
                    ));
                }
                Ok(BoolExp::CompareColumns {
                    column: col.name.clone(),
                    op,
                    other,
                })
            }
            "cast" => self.cast(table, root, col, v),
            "ancestor" | "ancestor_any" | "descendant" | "descendant_any" | "matches"
            | "matches_any" | "matches_fulltext" => {
                match crate::operators::ltree_op(name, v, ty, "$", &adapt) {
                    Some(op) => cmp(op.map_err(|e| e.message)?),
                    None => Err(format!("operator {op} is only supported on ltree columns")),
                }
            }
            _ if name.starts_with("st_") => {
                match crate::operators::spatial_op(name, v, ty, "$", &adapt) {
                    Some(op) => cmp(op.map_err(|e| e.message)?),
                    None if scalars::is_geo(ty) => Err(format!(
                        "the operator {op} is not supported in permissions yet"
                    )),
                    None => Err(format!(
                        "operator {op} is only supported on geometry and geography columns"
                    )),
                }
            }
            _ => Err(format!("Unknown operator: {op}")),
        }
    }

    /// `_cast: {<type>: {<ops>}}`: the operators applied to the column cast to `<type>`.
    fn cast(
        &self,
        table: &TableInfo,
        root: &TableInfo,
        col: &ColumnInfo,
        v: &Json,
    ) -> Result<BoolExp, PermError> {
        let Json::Object(targets) = v else {
            return Err(format!(
                "expected an object for _cast, but found {}",
                json_kind(v)
            ));
        };
        let mut items = vec![];
        for (target, ops) in targets {
            let to =
                crate::operators::permission_cast_target(&col.ty, target).ok_or_else(|| {
                    format!(
                        "cannot cast column of type \"{}\" to type \"{target}\"",
                        col.ty.name
                    )
                })?;
            let cast_col = ColumnInfo {
                ty: to.clone(),
                ..col.clone()
            };
            let exp = match ops {
                Json::Object(_) => self.column(table, root, &cast_col, ops)?,
                other => {
                    return Err(format!(
                        "expected an object of operators, but found {}",
                        json_kind(other)
                    ));
                }
            };
            items.push(cast_compares(exp, &col.ty, &to)?);
        }
        Ok(match items.len() {
            1 => items.pop().expect("one item"),
            _ => BoolExp::And(items),
        })
    }

    /// `_eq` / `_neq`. With `null_safe`, a null value compares like `IS [NOT] NULL` (V2
    /// renders `(c = v) OR (c IS NULL AND v IS NULL)`; session variables are never null, so
    /// only a literal null makes a difference).
    fn equality(
        &self,
        col: &ColumnInfo,
        v: &Json,
        null_safe: bool,
        negate: bool,
    ) -> Result<BoolExp, PermError> {
        let op = if null_safe && v.is_null() {
            CompareOp::IsNull(!negate)
        } else if negate {
            CompareOp::Neq(value(v, &col.ty)?)
        } else {
            CompareOp::Eq(value(v, &col.ty)?)
        };
        Ok(BoolExp::Compare {
            column: col.name.clone(),
            ty: col.ty.clone(),
            op,
        })
    }
}

/// Turn the comparisons of a cast column (`exp`, built against type `to`) into comparisons
/// of the column of type `ty` cast to `to`.
fn cast_compares(exp: BoolExp, ty: &ColumnType, to: &ColumnType) -> Result<BoolExp, PermError> {
    match exp {
        BoolExp::Compare { column, op, .. } => Ok(BoolExp::Compare {
            column,
            ty: ty.clone(),
            op: CompareOp::Cast {
                to: to.clone(),
                op: Box::new(op),
            },
        }),
        BoolExp::And(items) => Ok(BoolExp::And(
            items
                .into_iter()
                .map(|e| cast_compares(e, ty, to))
                .collect::<Result<_, _>>()?,
        )),
        _ => Err("column comparison operators cannot be used inside _cast".to_owned()),
    }
}

/// A preset or operator value: a session variable reference or a literal of type `ty`.
pub fn value(v: &Json, ty: &ColumnType) -> Result<Value, PermError> {
    if let Some(name) = v.as_str().and_then(session_variable) {
        return Ok(Value::Session {
            name,
            ty: ty.clone(),
        });
    }
    if ty.is_array() && !v.is_array() && !v.is_null() {
        return Err(format!(
            "expected an array of {} values, but found {}",
            scalars::scalar_name(&ty.element()),
            json_kind(v)
        ));
    }
    crate::lower::to_value(v, ty, "$").map_err(|e| e.message)
}

/// The lower-cased session variable a string refers to, if it is one (`X-Hasura-*`, or the
/// legacy `REQ_USER_ID` alias of `x-hasura-user-id`).
pub fn session_variable(s: &str) -> Option<String> {
    if hge_core::session::is_session_variable(s) {
        Some(s.to_ascii_lowercase())
    } else if s.eq_ignore_ascii_case("REQ_USER_ID") {
        Some("x-hasura-user-id".to_owned())
    } else {
        None
    }
}

/// `set` presets of an insert or update permission.
pub fn presets(
    table: &TableInfo,
    set: &indexmap::IndexMap<String, Json>,
) -> Result<indexmap::IndexMap<String, Value>, PermError> {
    let mut out = indexmap::IndexMap::with_capacity(set.len());
    for (name, v) in set {
        let col = table.columns.get(name).ok_or_else(|| {
            format!(
                "column \"{name}\" not found in table \"{}\"",
                table.display_name()
            )
        })?;
        out.insert(name.clone(), value(v, &col.ty)?);
    }
    Ok(out)
}

fn json_kind(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}
