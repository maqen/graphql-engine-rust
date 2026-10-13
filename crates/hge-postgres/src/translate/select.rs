//! Selects: rows as JSON, nested relationships as correlated subqueries.
//!
//! ```sql
//! SELECT coalesce(json_agg(row_to_json((SELECT "_e2" FROM (SELECT "_b1"."id" AS "id") AS "_e2"))
//!                          ORDER BY "_b1"."name" ASC NULLS LAST), '[]')
//! FROM (SELECT * FROM "public"."author" AS "_t1"
//!       WHERE … ORDER BY "_t1"."name" ASC NULLS LAST LIMIT 10) AS "_b1"
//! ```

use hge_core::{Code, ColumnType, Error, QualifiedTable, Session};
use hge_ir::{
    BoolExp, Field, FunctionArgValue, FunctionCall, JsonPathElement, NativeQueryPart, NullsOrder,
    OrderBy, OrderByTarget, OrderDirection, RowSource, Select, SelectArgs, Value,
};

use crate::sql::SqlBuilder;

pub struct Translator<'s> {
    pub b: SqlBuilder,
    session: &'s Session,
    next_alias: usize,
}

/// Postgres truncates longer identifiers (NAMEDATALEN - 1).
const MAX_IDENTIFIER_LEN: usize = 63;

/// `json_build_object` takes at most 100 arguments.
pub(crate) const MAX_JSON_BUILD_PAIRS: usize = 50;

/// How a nested select is joined to its parent row: `(parent alias, [(parent col, child col)])`.
pub type Join<'a> = Option<(&'a str, &'a [(String, String)])>;

/// The rows a select reads: a table (or a function returning its rows), the request's
/// arguments and the permission filter.
pub(crate) struct BaseQuery<'a> {
    pub table: &'a QualifiedTable,
    pub from: Option<&'a RowSource>,
    pub args: &'a SelectArgs,
    pub permission_filter: Option<&'a BoolExp>,
}

impl<'a> From<&'a Select> for BaseQuery<'a> {
    fn from(sel: &'a Select) -> Self {
        BaseQuery {
            table: &sel.table,
            from: sel.from.as_ref(),
            args: &sel.args,
            permission_filter: sel.permission_filter.as_ref(),
        }
    }
}

impl<'s> Translator<'s> {
    pub fn new(session: &'s Session) -> Self {
        Self::with_builder(session, SqlBuilder::new())
    }

    /// A translator writing into `b` (e.g. a [`SqlBuilder::multiplexed`] one).
    pub fn with_builder(session: &'s Session, b: SqlBuilder) -> Self {
        Translator {
            b,
            session,
            next_alias: 0,
        }
    }

    pub(crate) fn fresh(&mut self, prefix: &str) -> String {
        self.next_alias += 1;
        format!("{prefix}{}", self.next_alias)
    }

    /// Aliases of a select's rows (`_bN`) and of its table inside the base query (`_tN`).
    pub(crate) fn base_aliases(&mut self) -> (String, String) {
        self.next_alias += 1;
        let n = self.next_alias;
        (format!("_b{n}"), format!("_t{n}"))
    }

    /// A JSON array of rows.
    pub fn select_many(&mut self, sel: &Select, join: Join<'_>) -> Result<(), Error> {
        let (base, inner) = self.base_aliases();

        self.b.push("SELECT coalesce(json_agg(");
        self.row_json(&sel.fields, &base)?;
        self.node_order(&sel.args.order_by, &base);
        self.b.push("), '[]') FROM (");
        let limit = effective_limit(sel.args.limit, sel.permission_limit);
        self.base(sel.into(), &inner, join, limit)?;
        self.b.push(") AS ").ident(&base);
        Ok(())
    }

    /// A single JSON object (or SQL NULL when there is no row).
    pub fn select_one(&mut self, sel: &Select, join: Join<'_>) -> Result<(), Error> {
        let (base, inner) = self.base_aliases();

        self.b.push("SELECT ");
        self.row_json(&sel.fields, &base)?;
        self.b.push(" FROM (");
        let limit = effective_limit(sel.args.limit, Some(1));
        self.base(sel.into(), &inner, join, limit)?;
        self.b.push(") AS ").ident(&base);
        Ok(())
    }

    /// ` ORDER BY …` of the rows of `base` (the output of [`Self::base`]), for `json_agg`.
    pub(crate) fn node_order(&mut self, order: &[OrderBy], base: &str) {
        if order.is_empty() {
            return;
        }
        self.b.push(" ORDER BY ");
        for (i, ob) in order.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            match &ob.target {
                OrderByTarget::Column(c) => {
                    self.b.qualified(base, c);
                }
                _ => {
                    self.b.qualified(base, &order_key(i));
                }
            }
            push_direction(&mut self.b, ob);
        }
    }

    /// `SELECT * FROM table WHERE … ORDER BY … LIMIT … OFFSET …`
    pub(crate) fn base(
        &mut self,
        sel: BaseQuery<'_>,
        alias: &str,
        join: Join<'_>,
        limit: Option<u64>,
    ) -> Result<(), Error> {
        let args = sel.args;
        self.b.push("SELECT ");
        if !args.distinct_on.is_empty() {
            self.b.push("DISTINCT ON (");
            for (i, target) in args.distinct_on.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                match target {
                    OrderByTarget::Column(c) => {
                        self.b.qualified(alias, c);
                    }
                    // Must match the initial ORDER BY items, which are order-key output
                    // columns for expressions (re-rendering would bind fresh parameters).
                    _ if i < args.order_by.len() => {
                        self.b.ident(&order_key(i));
                    }
                    _ => self.order_target(target, alias)?,
                }
            }
            self.b.push(") ");
        }
        self.b.ident(alias).push(".*");
        for (i, ob) in args.order_by.iter().enumerate() {
            if !matches!(&ob.target, OrderByTarget::Column(_)) {
                self.b.push(", ");
                self.order_target(&ob.target, alias)?;
                self.b.push(" AS ").ident(&order_key(i));
            }
        }
        self.b.push(" FROM ");
        match sel.from {
            // Arguments refer to the parent row (a computed field's table argument).
            Some(RowSource::Function(call)) => {
                self.function_call(call, join.map_or(alias, |(parent, _)| parent))?
            }
            Some(RowSource::NativeQuery(parts)) => self.native_query(parts)?,
            None => {
                self.b
                    .ident(&sel.table.schema)
                    .push(".")
                    .ident(&sel.table.name);
            }
        }
        self.b.push(" AS ").ident(alias);

        let mut conds = 0;
        let mut and = |b: &mut SqlBuilder| {
            b.push(if conds == 0 { " WHERE " } else { " AND " });
            conds += 1;
        };
        if let Some((parent, mapping)) = join {
            for (parent_col, child_col) in mapping {
                and(&mut self.b);
                self.b.push("(");
                self.b.qualified(alias, child_col).push(" = ");
                self.b.qualified(parent, parent_col).push(")");
            }
        }
        // Permission filter first, as V2 renders it.
        if let Some(p) = sel.permission_filter {
            and(&mut self.b);
            self.bool_exp(p, alias)?;
        }
        if let Some(w) = &args.where_ {
            and(&mut self.b);
            self.bool_exp(w, alias)?;
        }

        // Ordering inside the base query only matters when rows are cut.
        let cut = limit.is_some() || args.offset.is_some() || !args.distinct_on.is_empty();
        if cut && !args.order_by.is_empty() {
            self.b.push(" ORDER BY ");
            for (i, ob) in args.order_by.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                match &ob.target {
                    OrderByTarget::Column(c) => {
                        self.b.qualified(alias, c);
                    }
                    _ => {
                        self.b.ident(&order_key(i));
                    }
                }
                push_direction(&mut self.b, ob);
            }
        }
        if let Some(l) = limit {
            // Bound, not inlined: every distinct limit would otherwise be a new statement in
            // each connection's prepared-statement cache.
            self.b.push(" LIMIT ");
            self.b
                .param(Some(l.to_string()), &ColumnType::builtin("int8"));
        }
        if let Some(o) = args.offset {
            self.b.push(" OFFSET ");
            self.b
                .param(Some(o.to_string()), &ColumnType::builtin("int8"));
        }
        Ok(())
    }

    /// An order-by target as a scalar expression on rows of `alias`.
    fn order_target(&mut self, target: &OrderByTarget, alias: &str) -> Result<(), Error> {
        match target {
            OrderByTarget::Column(c) => {
                self.b.qualified(alias, c);
            }
            OrderByTarget::RedactedColumn { column, condition } => {
                self.b.push("(CASE WHEN ");
                self.bool_exp(condition, alias)?;
                self.b.push(" THEN ");
                self.b.qualified(alias, column);
                self.b.push(" ELSE NULL END)");
            }
            OrderByTarget::ComputedField(call) => self.function_call(call, alias)?,
            OrderByTarget::ObjectRelationship {
                table,
                mapping,
                filter,
                target,
            } => {
                let rel_alias = self.fresh("_o");
                self.b.push("(SELECT ");
                self.order_target(target, &rel_alias)?;
                self.b.push(" FROM ");
                self.b.ident(&table.schema).push(".").ident(&table.name);
                self.b.push(" AS ").ident(&rel_alias).push(" WHERE ");
                for (i, (local, remote)) in mapping.iter().enumerate() {
                    if i > 0 {
                        self.b.push(" AND ");
                    }
                    self.b.qualified(&rel_alias, remote).push(" = ");
                    self.b.qualified(alias, local);
                }
                if let Some(filter) = filter {
                    self.b.push(" AND ");
                    self.bool_exp(filter, &rel_alias)?;
                }
                self.b.push(" LIMIT 1)");
            }
            OrderByTarget::ArrayAggregate {
                table,
                mapping,
                permission_filter,
                aggregate,
            } => {
                let filter = permission_filter.as_deref();
                self.aggregate_order_target(table, mapping, filter, aggregate, alias)?;
            }
        }
        Ok(())
    }

    /// The JSON object for one row of `alias`. `row_to_json` keeps key order, has no
    /// argument limit, and produces compact output. Postgres truncates identifiers to 63
    /// bytes, so objects with longer keys use `json_build_object` with literal keys.
    pub(crate) fn row_json(
        &mut self,
        fields: &[(String, Field)],
        alias: &str,
    ) -> Result<(), Error> {
        if fields.iter().any(|(key, _)| key.len() > MAX_IDENTIFIER_LEN) {
            if fields.len() > MAX_JSON_BUILD_PAIRS {
                return Err(Error::validation(format!(
                    "a selection with more than {MAX_JSON_BUILD_PAIRS} fields cannot use \
                     aliases longer than {MAX_IDENTIFIER_LEN} bytes"
                )));
            }
            self.b.push("json_build_object(");
            for (i, (key, field)) in fields.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                self.b.string_literal(key);
                self.b.push(", ");
                self.field(field, alias)?;
            }
            self.b.push(")");
            return Ok(());
        }
        let e = self.fresh("_e");
        self.b
            .push("row_to_json((SELECT ")
            .ident(&e)
            .push(" FROM (SELECT ");
        for (i, (key, field)) in fields.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            self.field(field, alias)?;
            self.b.push(" AS ").ident(key);
        }
        self.b.push(") AS ").ident(&e).push("))");
        Ok(())
    }

    fn field(&mut self, field: &Field, alias: &str) -> Result<(), Error> {
        match field {
            Field::Typename(t) => {
                self.b.string_literal(t);
            }
            Field::Column(c) => {
                if let Some(condition) = &c.redact {
                    self.b.push("(CASE WHEN ");
                    self.bool_exp(condition, alias)?;
                    self.b.push(" THEN ");
                }
                let geo =
                    c.ty.dimensions == 0 && matches!(c.ty.name.as_str(), "geometry" | "geography");
                if let Some(path) = &c.json_path
                    && !path.is_empty()
                {
                    self.b.push("(");
                    self.b.qualified(alias, &c.column).push(" #> ");
                    self.json_path_param(path);
                    self.b.push(")");
                } else if c.stringify {
                    self.b.push("(");
                    self.b.qualified(alias, &c.column).push(")::text");
                } else if geo {
                    self.b.push("ST_AsGeoJSON(");
                    self.b.qualified(alias, &c.column).push(", 15, 4)::json");
                } else {
                    self.b.qualified(alias, &c.column);
                }
                if c.redact.is_some() {
                    self.b.push(" ELSE NULL END)");
                }
            }
            Field::ObjectRelationship(rel) => {
                self.b.push("(");
                self.select_one(&rel.select, Some((alias, &rel.mapping)))?;
                self.b.push(")");
            }
            Field::ArrayRelationship(rel) => {
                self.b.push("(");
                self.select_many(&rel.select, Some((alias, &rel.mapping)))?;
                self.b.push(")");
            }
            Field::ArrayAggregate(rel) => {
                self.b.push("(");
                self.select_aggregate(&rel.select, Some((alias, &rel.mapping)))?;
                self.b.push(")");
            }
            Field::ComputedScalar(cf) => {
                let geo = cf.ty.dimensions == 0
                    && matches!(cf.ty.name.as_str(), "geometry" | "geography");
                if let Some(path) = cf.json_path.as_deref().filter(|p| !p.is_empty()) {
                    self.b.push("(");
                    self.function_call(&cf.call, alias)?;
                    self.b.push(" #> ");
                    self.json_path_param(path);
                    self.b.push(")");
                } else if cf.stringify {
                    self.b.push("(");
                    self.function_call(&cf.call, alias)?;
                    self.b.push(")::text");
                } else if geo {
                    self.b.push("ST_AsGeoJSON(");
                    self.function_call(&cf.call, alias)?;
                    self.b.push(", 15, 4)::json");
                } else {
                    self.function_call(&cf.call, alias)?;
                }
            }
            Field::ComputedTable(select) => {
                self.b.push("(");
                self.select_many(select, Some((alias, &[])))?;
                self.b.push(")");
            }
        }
        Ok(())
    }

    fn json_path_param(&mut self, path: &[JsonPathElement]) {
        let elems: Vec<String> = path
            .iter()
            .map(|e| match e {
                JsonPathElement::Key(k) => k.clone(),
                JsonPathElement::Index(i) => i.to_string(),
            })
            .collect();
        let literal = crate::translate::boolexp::array_literal(&elems);
        self.b
            .param(Some(literal), &ColumnType::builtin("text").array_of());
    }

    /// `"schema"."fn"(args)`; a row argument is the row of `alias`.
    pub(crate) fn function_call(&mut self, call: &FunctionCall, alias: &str) -> Result<(), Error> {
        self.b
            .ident(&call.function.schema)
            .push(".")
            .ident(&call.function.name)
            .push("(");
        for (i, arg) in call.args.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            if let Some(name) = &arg.name {
                self.b.ident(name).push(" => ");
            }
            match &arg.value {
                FunctionArgValue::Value(v) => self.value(v)?,
                FunctionArgValue::Row { table, columns } => {
                    self.b.push("ROW(");
                    for (j, c) in columns.iter().enumerate() {
                        if j > 0 {
                            self.b.push(", ");
                        }
                        self.b.qualified(alias, c);
                    }
                    self.b
                        .push(")::")
                        .ident(&table.schema)
                        .push(".")
                        .ident(&table.name);
                }
                FunctionArgValue::Session => {
                    // Sorted keys, as V2 passes them (and one cohort per distinct session).
                    let json = self.session.variables.to_sorted_json_string();
                    self.b.param(Some(json), &ColumnType::builtin("json"));
                }
            }
        }
        self.b.push(")");
        Ok(())
    }

    /// A native query's code as a subquery, its arguments bound as parameters. The newline
    /// before the closing parenthesis ends a trailing `--` comment in the code.
    fn native_query(&mut self, parts: &[NativeQueryPart]) -> Result<(), Error> {
        self.b.push("(");
        for part in parts {
            match part {
                NativeQueryPart::Sql(sql) => {
                    self.b.push(sql);
                }
                NativeQueryPart::Arg(v) => self.value(v)?,
            }
        }
        self.b.push("\n)");
        Ok(())
    }

    /// Bind an IR value as a parameter.
    pub(crate) fn value(&mut self, v: &Value) -> Result<(), Error> {
        match v {
            Value::Literal { text, ty } => {
                self.b.input_param(text.clone(), ty);
            }
            Value::Session { name, ty } => {
                let text = self.session.variables.get(name).ok_or_else(|| {
                    Error::new(
                        Code::NotFound,
                        format!("missing session variable: \"{name}\""),
                    )
                })?;
                // V2 reads geometries in session variables as GeoJSON too
                // (`withConstructorFn`).
                self.b.input_param(Some(text.to_owned()), ty);
            }
        }
        Ok(())
    }
}

pub(crate) fn order_key(i: usize) -> String {
    // Lives next to the table's own columns in the base query; keep it unlikely to clash.
    format!("__hge_order_{i}")
}

fn push_direction(b: &mut SqlBuilder, ob: &OrderBy) {
    b.push(match ob.direction {
        OrderDirection::Asc => " ASC",
        OrderDirection::Desc => " DESC",
    });
    b.push(match ob.nulls {
        NullsOrder::First => " NULLS FIRST",
        NullsOrder::Last => " NULLS LAST",
    });
}

pub(crate) fn effective_limit(requested: Option<u64>, permission: Option<u64>) -> Option<u64> {
    match (requested, permission) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
