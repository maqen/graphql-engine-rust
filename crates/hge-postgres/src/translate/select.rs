//! Selects: rows as JSON, nested relationships as correlated subqueries.
//!
//! ```sql
//! SELECT coalesce(json_agg(row_to_json((SELECT "_e2" FROM (SELECT "_b1"."id" AS "id") AS "_e2"))
//!                          ORDER BY "_b1"."name" ASC NULLS LAST), '[]')
//! FROM (SELECT * FROM "public"."author" AS "_t1"
//!       WHERE … ORDER BY "_t1"."name" ASC NULLS LAST LIMIT 10) AS "_b1"
//! ```

use hge_core::{Code, ColumnType, Error, Session};
use hge_ir::{
    Field, JsonPathElement, NullsOrder, OrderBy, OrderByTarget, OrderDirection, Select, Value,
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
const MAX_JSON_BUILD_PAIRS: usize = 50;

/// How a nested select is joined to its parent row: `(parent alias, [(parent col, child col)])`.
pub type Join<'a> = Option<(&'a str, &'a [(String, String)])>;

impl<'s> Translator<'s> {
    pub fn new(session: &'s Session) -> Self {
        Translator {
            b: SqlBuilder::new(),
            session,
            next_alias: 0,
        }
    }

    pub(crate) fn fresh(&mut self, prefix: &str) -> String {
        self.next_alias += 1;
        format!("{prefix}{}", self.next_alias)
    }

    /// A JSON array of rows.
    pub fn select_many(&mut self, sel: &Select, join: Join<'_>) -> Result<(), Error> {
        let n = self.next_alias + 1;
        let (base, inner) = (format!("_b{n}"), format!("_t{n}"));
        self.next_alias = n;

        self.b.push("SELECT coalesce(json_agg(");
        self.row_json(sel, &base)?;
        let order = &sel.args.order_by;
        if !order.is_empty() {
            self.b.push(" ORDER BY ");
            for (i, ob) in order.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                match &ob.target {
                    OrderByTarget::Column(c) => {
                        self.b.qualified(&base, c);
                    }
                    OrderByTarget::ObjectRelationship { .. } => {
                        self.b.qualified(&base, &order_key(i));
                    }
                }
                push_direction(&mut self.b, ob);
            }
        }
        self.b.push("), '[]') FROM (");
        let limit = effective_limit(sel.args.limit, sel.permission_limit);
        self.base(sel, &inner, join, limit)?;
        self.b.push(") AS ").ident(&base);
        Ok(())
    }

    /// A single JSON object (or SQL NULL when there is no row).
    pub fn select_one(&mut self, sel: &Select, join: Join<'_>) -> Result<(), Error> {
        let n = self.next_alias + 1;
        let (base, inner) = (format!("_b{n}"), format!("_t{n}"));
        self.next_alias = n;

        self.b.push("SELECT ");
        self.row_json(sel, &base)?;
        self.b.push(" FROM (");
        self.base(sel, &inner, join, Some(1))?;
        self.b.push(") AS ").ident(&base);
        Ok(())
    }

    /// `SELECT * FROM table WHERE … ORDER BY … LIMIT … OFFSET …`
    fn base(
        &mut self,
        sel: &Select,
        alias: &str,
        join: Join<'_>,
        limit: Option<u64>,
    ) -> Result<(), Error> {
        let args = &sel.args;
        self.b.push("SELECT ");
        if !args.distinct_on.is_empty() {
            self.b.push("DISTINCT ON (");
            for (i, c) in args.distinct_on.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                self.b.qualified(alias, c);
            }
            self.b.push(") ");
        }
        self.b.ident(alias).push(".*");
        for (i, ob) in args.order_by.iter().enumerate() {
            if let OrderByTarget::ObjectRelationship { .. } = &ob.target {
                self.b.push(", ");
                self.order_target(&ob.target, alias)?;
                self.b.push(" AS ").ident(&order_key(i));
            }
        }
        self.b.push(" FROM ");
        self.b
            .ident(&sel.table.schema)
            .push(".")
            .ident(&sel.table.name);
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
        if let Some(w) = &args.where_ {
            and(&mut self.b);
            self.bool_exp(w, alias)?;
        }
        if let Some(p) = &sel.permission_filter {
            and(&mut self.b);
            self.bool_exp(p, alias)?;
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
                    OrderByTarget::ObjectRelationship { .. } => {
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
            OrderByTarget::ObjectRelationship {
                table,
                mapping,
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
                self.b.push(" LIMIT 1)");
            }
        }
        Ok(())
    }

    /// The JSON object for one row of `alias`. `row_to_json` keeps key order, has no
    /// argument limit, and produces compact output. Postgres truncates identifiers to 63
    /// bytes, so objects with longer keys use `json_build_object` with literal keys.
    fn row_json(&mut self, sel: &Select, alias: &str) -> Result<(), Error> {
        if sel
            .fields
            .iter()
            .any(|(key, _)| key.len() > MAX_IDENTIFIER_LEN)
        {
            if sel.fields.len() > MAX_JSON_BUILD_PAIRS {
                return Err(Error::validation(format!(
                    "a selection with more than {MAX_JSON_BUILD_PAIRS} fields cannot use \
                     aliases longer than {MAX_IDENTIFIER_LEN} bytes"
                )));
            }
            self.b.push("json_build_object(");
            for (i, (key, field)) in sel.fields.iter().enumerate() {
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
        for (i, (key, field)) in sel.fields.iter().enumerate() {
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
                let geo =
                    c.ty.dimensions == 0 && matches!(c.ty.name.as_str(), "geometry" | "geography");
                if let Some(path) = &c.json_path
                    && !path.is_empty()
                {
                    self.b.push("(");
                    self.b.qualified(alias, &c.column).push(" #> ");
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
        }
        Ok(())
    }

    /// Bind an IR value as a parameter.
    pub(crate) fn value(&mut self, v: &Value) -> Result<(), Error> {
        match v {
            Value::Literal { text, ty } => {
                self.b.param(text.clone(), ty);
            }
            Value::Session { name, ty } => {
                let text = self.session.variables.get(name).ok_or_else(|| {
                    Error::new(
                        Code::NotFound,
                        format!("missing session variable: \"{name}\""),
                    )
                })?;
                self.b.param(Some(text.to_owned()), ty);
            }
        }
        Ok(())
    }
}

fn order_key(i: usize) -> String {
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

fn effective_limit(requested: Option<u64>, permission: Option<u64>) -> Option<u64> {
    match (requested, permission) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
