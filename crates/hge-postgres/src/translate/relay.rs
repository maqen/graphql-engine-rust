//! Relay connections and node ids.
//!
//! A connection numbers the rows of its base query in order, keeps the page (`after` /
//! `before` comparisons on the order-by values, then `first` / `last`), and builds the
//! connection object from the page:
//!
//! ```sql
//! WITH "_c1_base" AS (
//!        SELECT row_number() OVER (ORDER BY "_b2"."title" ASC NULLS LAST, …) AS "__row_number",
//!               (json_build_object('title', "_b2"."title", 'id', "_b2"."id"))::text AS "__cursor",
//!               "_b2"."title" AS "__split_0", …, row_to_json(…) AS "__node_0"
//!        FROM (SELECT * FROM "public"."article" AS "_t2" WHERE …) AS "_b2"),
//!      "_c1_page" AS (SELECT * FROM "_c1_base" WHERE <after/before>
//!                     ORDER BY "__row_number" DESC LIMIT $1)
//! SELECT json_build_object(
//!          'pageInfo', json_build_object('hasNextPage', EXISTS (SELECT 1 FROM "_c1_base"
//!                        WHERE "__row_number" > (SELECT max("__row_number") FROM "_c1_page")), …),
//!          'edges', (SELECT coalesce(json_agg(json_build_object('cursor', <base64 "__cursor">,
//!                                                              'node', "__node_0")
//!                                             ORDER BY "__row_number"), '[]')
//!                    FROM "_c1_page"))
//! ```
//!
//! Cursors and node ids are base64 of the JSON text Postgres produces, exactly as V2 builds
//! them (`json_build_object` / `json_build_array` spacing included).

use hge_core::{ColumnType, Error};
use hge_ir::{
    ConnectionField, ConnectionSelect, ConnectionSlice, EdgeField, NodeIdField, OrderByTarget,
    OrderDirection, PageInfoField, SplitKey, SplitKind,
};

use super::Translator;
use super::select::{Join, MAX_JSON_BUILD_PAIRS, order_key};

impl Translator<'_> {
    /// A connection object (`{"pageInfo": …, "edges": […]}`) over the rows of `conn`.
    pub fn select_connection(
        &mut self,
        conn: &ConnectionSelect,
        join: Join<'_>,
    ) -> Result<(), Error> {
        let (base, inner) = self.base_aliases();
        let cte = self.fresh("_c");
        let base_cte = format!("{cte}_base");
        let page_cte = format!("{cte}_page");
        let sel = &conn.select;
        let order = &sel.args.order_by;

        // -- numbered rows ---------------------------------------------------------------
        self.b
            .push("WITH ")
            .ident(&base_cte)
            .push(" AS (SELECT row_number() OVER (");
        if !order.is_empty() {
            self.b.push("ORDER BY ");
            for (i, ob) in order.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                self.order_value(i, &ob.target, &base);
                push_direction(&mut self.b, ob.direction, ob.nulls);
            }
        }
        self.b.push(") AS \"__row_number\", (");
        self.cursor_object(conn, &base)?;
        self.b.push(")::text AS \"__cursor\"");
        if let Some(split) = &conn.split {
            for (i, item) in split.items.iter().enumerate() {
                self.b.push(", ");
                match &item.key {
                    SplitKey::OrderBy(j) => {
                        let target = &order
                            .get(*j)
                            .ok_or_else(|| Error::unexpected("cursor item without order_by"))?
                            .target;
                        self.order_value(*j, target, &base);
                    }
                    SplitKey::Column(c) => {
                        self.b.qualified(&base, c);
                    }
                }
                self.b.push(" AS ").ident(&split_column(i));
            }
        }
        let mut node = 0;
        for (_, field) in &conn.fields {
            if let ConnectionField::Edges(edges) = field {
                for (_, edge) in edges {
                    if let EdgeField::Node(fields) = edge {
                        self.b.push(", ");
                        self.row_json(fields, &base)?;
                        self.b.push(" AS ").ident(&node_column(node));
                        node += 1;
                    }
                }
            }
        }
        self.b.push(" FROM (");
        // The permission limit cuts the rows before paging (V2 has no `limit` argument on
        // connections).
        self.base(sel.into(), &inner, join, sel.permission_limit)?;
        self.b.push(") AS ").ident(&base).push(")");

        // -- the page --------------------------------------------------------------------
        self.b
            .push(", ")
            .ident(&page_cte)
            .push(" AS (SELECT * FROM ")
            .ident(&base_cte);
        if let Some(split) = &conn.split {
            self.b.push(" WHERE ");
            self.split_condition(split.kind, &split.items, 0)?;
        }
        match conn.slice {
            None => {}
            Some(ConnectionSlice::First(n)) => {
                self.b.push(" ORDER BY \"__row_number\" ASC LIMIT ");
                self.b
                    .param(Some(n.to_string()), &ColumnType::builtin("int8"));
            }
            Some(ConnectionSlice::Last(n)) => {
                self.b.push(" ORDER BY \"__row_number\" DESC LIMIT ");
                self.b
                    .param(Some(n.to_string()), &ColumnType::builtin("int8"));
            }
        }
        self.b.push(") SELECT ");

        // -- the connection object -------------------------------------------------------
        let mut node = 0;
        json_object(self, &conn.fields, |t, field| {
            match field {
                ConnectionField::Typename(name) => {
                    t.b.string_literal(name);
                }
                ConnectionField::PageInfo(fields) => {
                    json_object(t, fields, |t, f| {
                        t.page_info_field(f, &base_cte, &page_cte);
                        Ok(())
                    })?;
                }
                ConnectionField::Edges(edges) => {
                    t.b.push("(SELECT coalesce(json_agg(");
                    json_object(t, edges, |t, edge| {
                        match edge {
                            EdgeField::Typename(name) => {
                                t.b.string_literal(name);
                            }
                            EdgeField::Cursor => base64(t, |t| {
                                t.b.push("\"__cursor\"");
                            }),
                            EdgeField::Node(_) => {
                                t.b.ident(&node_column(node));
                                node += 1;
                            }
                        }
                        Ok(())
                    })?;
                    t.b.push(" ORDER BY \"__row_number\" ASC), '[]') FROM ")
                        .ident(&page_cte)
                        .push(")");
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// The value an order-by item sorts on, for a row of the base query `base`.
    fn order_value(&mut self, i: usize, target: &OrderByTarget, base: &str) {
        match target {
            OrderByTarget::Column(c) => self.b.qualified(base, c),
            _ => self.b.qualified(base, &order_key(i)),
        };
    }

    /// The cursor object of a row: the order-by values by key path, or the primary key.
    fn cursor_object(&mut self, conn: &ConnectionSelect, base: &str) -> Result<(), Error> {
        let order = &conn.select.args.order_by;
        if order.is_empty() {
            self.b.push("json_build_object(");
            for (i, (column, _)) in conn.primary_key.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                self.b.string_literal(column).push(", ");
                self.b.qualified(base, column);
            }
            self.b.push(")");
            return Ok(());
        }
        if conn.cursor_paths.len() != order.len() {
            return Err(Error::unexpected(
                "connection cursor paths do not match order_by",
            ));
        }
        if order.len() > MAX_JSON_BUILD_PAIRS {
            return Err(Error::validation(format!(
                "a connection can be ordered by at most {MAX_JSON_BUILD_PAIRS} columns"
            )));
        }
        self.b.push("json_build_object(");
        for (i, (ob, path)) in order.iter().zip(&conn.cursor_paths).enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            let Some((last, parents)) = path.split_last() else {
                return Err(Error::unexpected("empty connection cursor path"));
            };
            for key in parents {
                self.b.string_literal(key).push(", json_build_object(");
            }
            self.b.string_literal(last).push(", ");
            self.order_value(i, &ob.target, base);
            for _ in parents {
                self.b.push(")");
            }
        }
        self.b.push(")");
        Ok(())
    }

    /// `after`: rows that sort after the cursor, `before`: rows that sort before it, as V2
    /// compares them: `(a > $1) OR ((a = $1) AND ((b > $2) OR ((b = $2) AND false)))`.
    fn split_condition(
        &mut self,
        kind: SplitKind,
        items: &[hge_ir::SplitItem],
        i: usize,
    ) -> Result<(), Error> {
        let Some(item) = items.get(i) else {
            self.b.push("false");
            return Ok(());
        };
        let op =
            match (kind, item.direction) {
                (SplitKind::After, OrderDirection::Asc)
                | (SplitKind::Before, OrderDirection::Desc) => " > ",
                (SplitKind::After, OrderDirection::Desc)
                | (SplitKind::Before, OrderDirection::Asc) => " < ",
            };
        let column = split_column(i);
        self.b.push("((").ident(&column).push(op);
        self.value(&item.value)?;
        self.b.push(") OR ((").ident(&column).push(" = ");
        self.value(&item.value)?;
        self.b.push(") AND ");
        self.split_condition(kind, items, i + 1)?;
        self.b.push("))");
        Ok(())
    }

    fn page_info_field(&mut self, f: &PageInfoField, base_cte: &str, page_cte: &str) {
        let first_or_last = |t: &mut Self, dir: &str, col: &str| {
            t.b.push("(SELECT ");
            match col {
                "__cursor" => base64(t, |t| {
                    t.b.push("\"__cursor\"");
                }),
                _ => {
                    t.b.ident(col);
                }
            }
            t.b.push(" FROM ")
                .ident(page_cte)
                .push(" ORDER BY \"__row_number\" ")
                .push(dir)
                .push(" LIMIT 1)");
        };
        let exists = |t: &mut Self, op: &str, agg: &str| {
            t.b.push("EXISTS (SELECT 1 FROM ")
                .ident(base_cte)
                .push(" WHERE \"__row_number\" ")
                .push(op)
                .push(" (SELECT ")
                .push(agg)
                .push("(\"__row_number\") FROM ")
                .ident(page_cte)
                .push("))");
        };
        match f {
            PageInfoField::Typename(name) => {
                self.b.string_literal(name);
            }
            PageInfoField::StartCursor => first_or_last(self, "ASC", "__cursor"),
            PageInfoField::EndCursor => first_or_last(self, "DESC", "__cursor"),
            PageInfoField::HasPreviousPage => exists(self, "<", "min"),
            PageInfoField::HasNextPage => exists(self, ">", "max"),
        }
    }

    /// A row's Relay node id.
    pub(crate) fn node_id(&mut self, id: &NodeIdField, alias: &str) {
        base64(self, |t| {
            t.b.push("(json_build_array(1, ");
            t.b.string_literal(&id.schema).push(", ");
            t.b.string_literal(&id.table);
            for (column, _) in &id.primary_key {
                t.b.push(", ");
                t.b.qualified(alias, column);
            }
            t.b.push("))::text");
        });
    }

    /// `<rel>_connection` on a row of `alias`.
    pub(crate) fn relationship_connection(
        &mut self,
        rel: &hge_ir::RelationshipConnection,
        alias: &str,
    ) -> Result<(), Error> {
        self.b.push("(");
        self.select_connection(&rel.connection, Some((alias, &rel.mapping)))?;
        self.b.push(")");
        Ok(())
    }
}

/// Base64 of the UTF-8 text `text` writes, without line breaks, as V2 encodes cursors and
/// node ids.
fn base64(t: &mut Translator<'_>, text: impl FnOnce(&mut Translator<'_>)) {
    t.b.push("regexp_replace(encode(convert_to(");
    text(t);
    t.b.push(", 'UTF8'), 'base64'), '\\n', '', 'g')");
}

fn json_object<T>(
    t: &mut Translator<'_>,
    items: &[(String, T)],
    mut value: impl FnMut(&mut Translator<'_>, &T) -> Result<(), Error>,
) -> Result<(), Error> {
    if items.len() > MAX_JSON_BUILD_PAIRS {
        return Err(Error::validation(format!(
            "a connection selection can have at most {MAX_JSON_BUILD_PAIRS} fields"
        )));
    }
    t.b.push("json_build_object(");
    for (i, (key, item)) in items.iter().enumerate() {
        if i > 0 {
            t.b.push(", ");
        }
        t.b.string_literal(key).push(", ");
        value(t, item)?;
    }
    t.b.push(")");
    Ok(())
}

fn split_column(i: usize) -> String {
    format!("__split_{i}")
}

fn node_column(i: usize) -> String {
    format!("__node_{i}")
}

fn push_direction(b: &mut crate::sql::SqlBuilder, dir: OrderDirection, nulls: hge_ir::NullsOrder) {
    b.push(match dir {
        OrderDirection::Asc => " ASC",
        OrderDirection::Desc => " DESC",
    });
    b.push(match nulls {
        hge_ir::NullsOrder::First => " NULLS FIRST",
        hge_ir::NullsOrder::Last => " NULLS LAST",
    });
}
