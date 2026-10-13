//! Legacy RQL boolean expressions (`where` of `/v1/query` DML) → IR.
//!
//! The grammar is the one permissions use (`$`/`_` operators, `{col: value}` shorthand,
//! `$exists`, column comparisons), but values are plain literals (no session variables), and
//! every column and relationship used must be visible to the role: columns need select
//! permission, and a relationship condition only sees target rows the role may select (V2's
//! `convBoolExp` + `checkSelPerm`).

use hge_core::{Code, ColumnType};
use hge_ir::{
    BoolExp, ColumnCompareOp, ColumnRef, CompareOp, Field, OrderByTarget, Select, SelectPermission,
    Value,
};
use hge_metadata_api::ApiError;
use hge_schema::TableInfo;
use hge_schema::scalars;
use hge_schema::tables::ColumnInfo;
use serde_json::Value as Json;

use super::json::{Path, err400, kind, parse_failed, type_mismatch};
use super::lower::Lowerer;

impl Lowerer<'_> {
    /// Lower `where` on `table`. Permission errors are reported at `where_path` (V2 checks
    /// permissions after parsing the whole expression, without a finer path).
    pub fn bool_exp(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        v: &Json,
        where_path: &Path,
    ) -> Result<BoolExp, ApiError> {
        BoolExpLowerer {
            lw: self,
            root: table,
            root_perm: perm,
            where_path,
            redact: true,
        }
        .exp(table, perm, v, where_path)
    }
}

struct BoolExpLowerer<'a, 'l> {
    lw: &'a Lowerer<'l>,
    root: &'a TableInfo,
    root_perm: &'a SelectPermission,
    where_path: &'a Path,
    /// Apply inherited-role redaction to compared columns (off inside `$exists`, whose
    /// columns are checked against another table's permission).
    redact: bool,
}

impl BoolExpLowerer<'_, '_> {
    /// `perm` is the select permission columns of `table` are checked against.
    fn exp(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        v: &Json,
        path: &Path,
    ) -> Result<BoolExp, ApiError> {
        let Json::Object(obj) = v else {
            return Err(type_mismatch("boolean expression", "Object", v, path));
        };
        let mut items = Vec::with_capacity(obj.len());
        for (key, value) in obj {
            let item_path = path.key(key);
            items.push(match key.as_str() {
                "$and" | "_and" | "$or" | "_or" => {
                    let Json::Array(parts) = value else {
                        return Err(type_mismatch("[]", "Array", value, &item_path));
                    };
                    let parts = parts
                        .iter()
                        .enumerate()
                        .map(|(i, p)| self.exp(table, perm, p, &item_path.index(i)))
                        .collect::<Result<Vec<_>, _>>()?;
                    if key.ends_with("and") {
                        BoolExp::And(parts)
                    } else {
                        BoolExp::Or(parts)
                    }
                }
                "$not" | "_not" => {
                    BoolExp::Not(Box::new(self.exp(table, perm, value, &item_path)?))
                }
                "$exists" | "_exists" => self.exists(perm, value, &item_path)?,
                _ => self.field(table, perm, key, value, path)?,
            });
        }
        Ok(match items.len() {
            1 => items.pop().expect("one item"),
            _ => BoolExp::And(items),
        })
    }

    /// V2 quirk: columns inside `_where` are checked against the enclosing table's select
    /// permission, not the `_table`'s.
    fn exists(&self, perm: &SelectPermission, v: &Json, path: &Path) -> Result<BoolExp, ApiError> {
        let o = super::json::Obj::new(v, path.clone(), "_exists")?;
        let name = super::json::table_name(o.req("_table")?, &o.at("_table"))?;
        let where_ = o.req("_where")?;
        let target = self.lw.table(&name).ok_or_else(|| {
            err400(
                Code::NotExists,
                format!(
                    "table \"{}\" does not exist",
                    super::json::display_table(&name)
                ),
                &o.at("_table"),
            )
        })?;
        let inner = BoolExpLowerer {
            redact: false,
            ..*self
        };
        Ok(BoolExp::Exists {
            table: target.name.clone(),
            exp: Box::new(inner.exp(target, perm, where_, &o.at("_where"))?),
        })
    }

    /// `key` is a column or a relationship of `table`; `path` is the enclosing object's path.
    fn field(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        key: &str,
        value: &Json,
        path: &Path,
    ) -> Result<BoolExp, ApiError> {
        if let Some(col) = table.columns.get(key) {
            self.lw.check_select_column(perm, key, self.where_path)?;
            let exp = self.column(table, perm, col, value, path)?;
            // Inherited roles compare a redacted column as
            // `CASE WHEN condition THEN col ELSE NULL END`, like V2's GraphQL API. V2's legacy
            // `where` compares the raw value; we redact so a filter cannot reveal hidden values.
            return Ok(match perm.redaction(key) {
                Some(condition) if self.redact => redact_comparisons(exp, condition),
                _ => exp,
            });
        }
        if let Some(rel) = table.relationship_by_name(key) {
            let target = self.lw.rel_target(rel)?;
            let item_path = path.key(key);
            let target_perm = self
                .lw
                .relationship_permission(rel, target, self.where_path)?;
            let exp = self.exp(target, target_perm, value, &item_path)?;
            let exp = match target_perm.row_filter() {
                Some(f) => BoolExp::And(vec![BoolExp::Rooted(Box::new(f.clone())), exp]),
                None => exp,
            };
            return Ok(BoolExp::Relationship {
                table: target.name.clone(),
                mapping: rel.mapping.clone(),
                exp: Box::new(exp),
            });
        }
        Err(err400(
            Code::NotExists,
            format!("\"{key}\" does not exist"),
            path,
        ))
    }

    fn column(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        col: &ColumnInfo,
        value: &Json,
        path: &Path,
    ) -> Result<BoolExp, ApiError> {
        if !col.ty.is_array() && col.ty.name == "json" {
            return Err(err400(
                Code::UnexpectedPayload,
                "JSON column can not be part of boolean expression",
                path,
            ));
        }
        let col_path = path.key(&col.name);
        let Json::Object(ops) = value else {
            // `{col: value}` always compares null-safely.
            return equality(col, value, true, false, &col_path);
        };
        let mut items = Vec::with_capacity(ops.len());
        for (op, v) in ops {
            items.push(self.operator(table, perm, col, op, v, &col_path.key(op))?);
        }
        Ok(match items.len() {
            1 => items.pop().expect("one item"),
            _ => BoolExp::And(items),
        })
    }

    fn operator(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        col: &ColumnInfo,
        op: &str,
        v: &Json,
        path: &Path,
    ) -> Result<BoolExp, ApiError> {
        let unknown = || {
            err400(
                Code::UnexpectedPayload,
                format!("Unknown operator: {op}"),
                path,
            )
        };
        let name = op
            .strip_prefix('$')
            .or_else(|| op.strip_prefix('_'))
            .ok_or_else(unknown)?;
        let ty = &col.ty;
        let text = ColumnType::builtin("text");
        let cmp = |op: CompareOp| {
            Ok(BoolExp::Compare {
                column: col.name.clone(),
                ty: ty.clone(),
                op,
            })
        };
        let guard = |ok: bool, expected: &str| {
            if ok {
                Ok(())
            } else {
                Err(err400(
                    Code::UnexpectedPayload,
                    format!(
                        " is of type \"{}\"; this operator works only on columns of type {expected}",
                        type_display(ty)
                    ),
                    path,
                ))
            }
        };
        let scalar = |v: &Json| literal(v, ty, path);
        match name {
            "eq" => equality(col, v, col.nullable, false, path),
            "ne" | "neq" => equality(col, v, col.nullable, true, path),
            "gt" => cmp(CompareOp::Gt(scalar(v)?)),
            "lt" => cmp(CompareOp::Lt(scalar(v)?)),
            "gte" => cmp(CompareOp::Gte(scalar(v)?)),
            "lte" => cmp(CompareOp::Lte(scalar(v)?)),
            "in" => cmp(CompareOp::In(list(v, ty, path)?)),
            "nin" => cmp(CompareOp::Nin(list(v, ty, path)?)),
            "like" | "nlike" | "ilike" | "nilike" | "similar" | "nsimilar" | "regex" | "nregex"
            | "iregex" | "niregex" => {
                guard(
                    scalars::is_string_type(ty),
                    "\"varchar\", \"text\", \"citext\", \"bpchar\"",
                )?;
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
                other => Err(parse_failed(
                    format!("expected Bool, but encountered {}", kind(other)),
                    path,
                )),
            },
            "contains" | "contained_in" => {
                guard(ty.is_array() || scalars::is_jsonb(ty), "an array or JSONB")?;
                let v = scalar(v)?;
                cmp(if name == "contains" {
                    CompareOp::Contains(v)
                } else {
                    CompareOp::ContainedIn(v)
                })
            }
            "has_key" | "has_keys_any" | "has_keys_all" => {
                guard(scalars::is_jsonb(ty), "\"jsonb\"")?;
                cmp(match name {
                    "has_key" => CompareOp::HasKey(literal(v, &text, path)?),
                    "has_keys_any" => CompareOp::HasKeysAny(list(v, &text, path)?),
                    _ => CompareOp::HasKeysAll(list(v, &text, path)?),
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
                let other = self.rhs_column(table, perm, col, v, path)?;
                Ok(BoolExp::CompareColumns {
                    column: col.name.clone(),
                    op,
                    other,
                })
            }
            "cast" | "jsonb_path_exists" | "jsonb_path_match" | "ancestor" | "ancestor_any"
            | "descendant" | "descendant_any" | "matches" | "matches_any" | "matches_fulltext" => {
                Err(not_supported_yet(op, path))
            }
            _ if name.starts_with("st_") => Err(not_supported_yet(op, path)),
            _ => Err(unknown()),
        }
    }

    /// The right-hand side of a column comparison: `"col"`, `["col"]` or `["$", "col"]`.
    fn rhs_column(
        &self,
        table: &TableInfo,
        perm: &SelectPermission,
        col: &ColumnInfo,
        v: &Json,
        path: &Path,
    ) -> Result<ColumnRef, ApiError> {
        // V2 checks only the left-hand column; checking the right-hand one too keeps a
        // comparison from revealing a column the role cannot select.
        let (other_table, other_perm, name, root) = match v {
            Json::String(c) => (table, perm, c, false),
            Json::Array(items) => match items.as_slice() {
                [] => {
                    return Err(err400(Code::Unexpected, "path cannot be empty", path));
                }
                [Json::String(c)] => (table, perm, c, false),
                [Json::String(d), Json::String(c)] if d == "$" => {
                    (self.root, self.root_perm, c, true)
                }
                _ => {
                    return Err(err400(
                        Code::NotSupported,
                        "Relationship references are not supported in column comparison RHS",
                        path,
                    ));
                }
            },
            _ => {
                return Err(err400(
                    Code::Unexpected,
                    "a boolean expression JSON must be either a string or an array",
                    path,
                ));
            }
        };
        let other = other_table.columns.get(name.as_str()).ok_or_else(|| {
            if other_table.relationship_by_name(name).is_some() {
                err400(
                    Code::UnexpectedPayload,
                    format!(
                        "expecting a database column; but, \"{name}\" is a relationship; column operators can only compare postgres columns"
                    ),
                    path,
                )
            } else {
                err400(
                    Code::NotExists,
                    format!("column \"{name}\" does not exist"),
                    path,
                )
            }
        })?;
        self.lw
            .check_select_column(other_perm, &other.name, self.where_path)?;
        // A column comparison reads both columns raw; with a redacted side it would compare
        // values the role cannot see.
        if self.redact {
            for (p, c) in [(perm, &col.name), (other_perm, &other.name)] {
                if p.redaction(c).is_some() {
                    return Err(err400(
                        Code::PermissionDenied,
                        format!(
                            "column \"{c}\" is only partially visible to role \"{}\" and cannot be used in a column comparison",
                            self.lw.role
                        ),
                        self.where_path,
                    ));
                }
            }
        }
        if other.ty != col.ty {
            return Err(err400(
                Code::UnexpectedPayload,
                format!(
                    "incompatible column types: \"{}\" has type \"{}\", but \"{}\" has type \"{}\"",
                    col.name,
                    type_display(&col.ty),
                    other.name,
                    type_display(&other.ty)
                ),
                path,
            ));
        }
        Ok(if root {
            ColumnRef::Root(name.clone())
        } else {
            ColumnRef::Current(name.clone())
        })
    }
}

/// `$eq` / `$neq`. A null-safe comparison with `null` becomes `IS [NOT] NULL`, like V2's
/// `(c = v) OR (c IS NULL AND v IS NULL)`.
fn equality(
    col: &ColumnInfo,
    v: &Json,
    null_safe: bool,
    negate: bool,
    path: &Path,
) -> Result<BoolExp, ApiError> {
    let compare = |op| BoolExp::Compare {
        column: col.name.clone(),
        ty: col.ty.clone(),
        op,
    };
    let value = literal(v, &col.ty, path)?;
    let cmp = compare(if negate {
        CompareOp::Neq(value)
    } else {
        CompareOp::Eq(value)
    });
    if !(null_safe && v.is_null()) {
        return Ok(cmp);
    }
    // V2 renders null-safe `c = v` as `(c = v) OR (c IS NULL AND v IS NULL)` (and `<>` as
    // `(c <> v) OR (c IS NOT NULL AND v IS NULL)`). For a null `v` that is three-valued:
    // NULL rather than false when the `IS [NOT] NULL` side fails, which shows under `_not`.
    Ok(BoolExp::Or(vec![cmp, compare(CompareOp::IsNull(!negate))]))
}

/// A literal of type `ty`.
pub fn literal(v: &Json, ty: &ColumnType, path: &Path) -> Result<Value, ApiError> {
    hge_schema::lower::to_value(v, ty, path.as_str()).map_err(|e| parse_failed(e.message, path))
}

/// A list of values of type `ty`, as one array-typed value.
fn list(v: &Json, ty: &ColumnType, path: &Path) -> Result<Value, ApiError> {
    if !v.is_array() {
        return Err(type_mismatch("[]", "Array", v, path));
    }
    literal(v, &ty.array_of(), path)
}

/// A type as V2 names it in messages (its `PGScalarType` rendering).
fn type_display(ty: &ColumnType) -> String {
    let mut name = match ty.name.as_str() {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "bool" => "boolean",
        "timestamp without time zone" => "timestamp",
        "timestamp with time zone" => "timestamptz",
        "time with time zone" => "timetz",
        "character varying" => "varchar",
        "character" => "bpchar",
        other => other,
    }
    .to_owned();
    for _ in 0..ty.dimensions {
        name.push_str("[]");
    }
    name
}

fn not_supported_yet(op: &str, path: &Path) -> ApiError {
    err400(
        Code::NotSupported,
        format!("the operator {op} is not supported on /v1/query yet"),
        path,
    )
}

/// Wrap every column comparison in `exp` (a lowered condition on one column) in
/// [`BoolExp::Redacted`]; the translator redacts single comparisons, not whole subtrees.
fn redact_comparisons(exp: BoolExp, condition: &BoolExp) -> BoolExp {
    match exp {
        BoolExp::And(items) => BoolExp::And(
            items
                .into_iter()
                .map(|e| redact_comparisons(e, condition))
                .collect(),
        ),
        BoolExp::Or(items) => BoolExp::Or(
            items
                .into_iter()
                .map(|e| redact_comparisons(e, condition))
                .collect(),
        ),
        BoolExp::Not(e) => BoolExp::Not(Box::new(redact_comparisons(*e, condition))),
        cmp @ BoolExp::Compare { .. } => BoolExp::Redacted {
            condition: Box::new(BoolExp::Rooted(Box::new(condition.clone()))),
            exp: Box::new(cmp),
        },
        other => other,
    }
}

/// Names of the session variables a boolean expression reads.
pub fn session_variables(exp: &BoolExp, out: &mut Vec<String>) {
    let mut exp = exp.clone();
    for_each_value(&mut exp, &mut |v| {
        if let Value::Session { name, .. } = v
            && !out.contains(name)
        {
            out.push(name.clone());
        }
    });
}

/// V2's `count` does not validate headers: a session variable the request lacks reads as
/// `NULL`.
pub fn nullify_missing_session_variables(exp: &mut BoolExp, session: &hge_core::Session) {
    for_each_value(exp, &mut |v| nullify_missing_value(v, session));
}

fn nullify_missing_value(v: &mut Value, session: &hge_core::Session) {
    if let Value::Session { name, ty } = v
        && session.variables.get(name).is_none()
    {
        *v = Value::null(ty.clone());
    }
}

/// [`nullify_missing_session_variables`] over every filter of a lowered select, including
/// relationship permission filters, which V2 reads with `current_setting` (NULL when unset).
pub fn nullify_missing_in_select(select: &mut Select, session: &hge_core::Session) {
    let filters = select
        .permission_filter
        .iter_mut()
        .chain(select.args.where_.iter_mut());
    for f in filters {
        nullify_missing_session_variables(f, session);
    }
    for ob in &mut select.args.order_by {
        let mut target = &mut ob.target;
        loop {
            match target {
                OrderByTarget::ObjectRelationship {
                    filter,
                    target: next,
                    ..
                } => {
                    if let Some(f) = filter {
                        nullify_missing_session_variables(f, session);
                    }
                    target = next;
                }
                OrderByTarget::RedactedColumn { condition, .. } => {
                    nullify_missing_session_variables(condition, session);
                    break;
                }
                OrderByTarget::ArrayAggregate {
                    permission_filter, ..
                } => {
                    if let Some(f) = permission_filter {
                        nullify_missing_session_variables(f, session);
                    }
                    break;
                }
                OrderByTarget::ComputedField(call) => {
                    for_each_call_value(call, &mut |v| nullify_missing_value(v, session));
                    break;
                }
                OrderByTarget::Column(_) => break,
            }
        }
    }
    for (_, field) in &mut select.fields {
        match field {
            Field::ObjectRelationship(rel) | Field::ArrayRelationship(rel) => {
                nullify_missing_in_select(&mut rel.select, session);
            }
            Field::Column(c) => {
                if let Some(cond) = &mut c.redact {
                    nullify_missing_session_variables(cond, session);
                }
            }
            Field::ComputedScalar(cf) => {
                for_each_call_value(&mut cf.call, &mut |v| nullify_missing_value(v, session));
            }
            Field::ComputedTable(sel) => nullify_missing_in_select(sel, session),
            // `/v1/query` builds no aggregate fields.
            Field::Typename(_) | Field::ArrayAggregate(_) => {}
        }
    }
}

fn for_each_value(exp: &mut BoolExp, f: &mut impl FnMut(&mut Value)) {
    match exp {
        BoolExp::And(items) | BoolExp::Or(items) => {
            for i in items {
                for_each_value(i, f);
            }
        }
        BoolExp::Not(e) | BoolExp::Rooted(e) => for_each_value(e, f),
        BoolExp::Redacted { condition, exp } => {
            for_each_value(condition, f);
            for_each_value(exp, f);
        }
        BoolExp::AggregationPredicate(p) => {
            for exp in [&mut p.permission_filter, &mut p.filter]
                .into_iter()
                .flatten()
            {
                for_each_value(exp, f);
            }
            for_each_value(&mut p.predicate, f);
        }
        BoolExp::Relationship { exp, .. } | BoolExp::Exists { exp, .. } => for_each_value(exp, f),
        BoolExp::ComputedTable { call, exp } => {
            for_each_call_value(call, f);
            for_each_value(exp, f);
        }
        BoolExp::CompareComputed { call, op, .. } => {
            for_each_call_value(call, f);
            op.for_each_value_mut(f);
        }
        BoolExp::CompareColumns { .. } => {}
        BoolExp::Compare { op, .. } => op.for_each_value_mut(f),
    }
}

fn for_each_call_value(call: &mut hge_ir::FunctionCall, f: &mut impl FnMut(&mut Value)) {
    for arg in &mut call.args {
        if let hge_ir::FunctionArgValue::Value(v) = &mut arg.value {
            f(v);
        }
    }
}
