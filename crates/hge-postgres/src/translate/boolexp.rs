//! Boolean expressions.

use hge_core::Error;
use hge_ir::{BoolExp, ColumnCompareOp, ColumnRef, CompareOp};

use super::Translator;

impl Translator<'_> {
    /// Render `exp` over rows of `alias`, which is also the root for `["$", col]` references.
    pub(crate) fn bool_exp(&mut self, exp: &BoolExp, alias: &str) -> Result<(), Error> {
        self.bool_exp_in(exp, alias, alias)
    }

    /// Render `exp` over rows of `alias`; root references resolve against `root`.
    fn bool_exp_in(&mut self, exp: &BoolExp, alias: &str, root: &str) -> Result<(), Error> {
        match exp {
            BoolExp::And(items) if items.is_empty() => {
                self.b.push("('true')");
            }
            BoolExp::Or(items) if items.is_empty() => {
                self.b.push("('false')");
            }
            BoolExp::And(items) | BoolExp::Or(items) => {
                let sep = if matches!(exp, BoolExp::And(_)) {
                    " AND "
                } else {
                    " OR "
                };
                self.b.push("(");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.b.push(sep);
                    }
                    self.bool_exp_in(item, alias, root)?;
                }
                self.b.push(")");
            }
            BoolExp::Not(inner) => {
                self.b.push("(NOT ");
                self.bool_exp_in(inner, alias, root)?;
                self.b.push(")");
            }
            BoolExp::Compare { column, op, .. } => self.compare(alias, column, op)?,
            BoolExp::Relationship {
                table,
                mapping,
                exp,
            } => {
                let rel = self.fresh("_e");
                self.b.push("EXISTS (SELECT 1 FROM ");
                self.b.ident(&table.schema).push(".").ident(&table.name);
                self.b.push(" AS ").ident(&rel).push(" WHERE ");
                for (local, remote) in mapping {
                    self.b.push("(");
                    self.b.qualified(&rel, remote).push(" = ");
                    self.b.qualified(alias, local).push(") AND ");
                }
                self.bool_exp_in(exp, &rel, root)?;
                self.b.push(")");
            }
            BoolExp::Exists { table, exp } => {
                let rel = self.fresh("_e");
                self.b.push("EXISTS (SELECT 1 FROM ");
                self.b.ident(&table.schema).push(".").ident(&table.name);
                self.b.push(" AS ").ident(&rel).push(" WHERE ");
                self.bool_exp_in(exp, &rel, root)?;
                self.b.push(")");
            }
            BoolExp::CompareColumns { column, op, other } => {
                self.b.push("(");
                self.b.qualified(alias, column);
                self.b.push(match op {
                    ColumnCompareOp::Eq => " = ",
                    ColumnCompareOp::Neq => " <> ",
                    ColumnCompareOp::Gt => " > ",
                    ColumnCompareOp::Lt => " < ",
                    ColumnCompareOp::Gte => " >= ",
                    ColumnCompareOp::Lte => " <= ",
                });
                match other {
                    ColumnRef::Current(c) => self.b.qualified(alias, c),
                    ColumnRef::Root(c) => self.b.qualified(root, c),
                };
                self.b.push(")");
            }
            BoolExp::Rooted(inner) => self.bool_exp_in(inner, alias, alias)?,
        }
        Ok(())
    }

    fn compare(&mut self, alias: &str, column: &str, op: &CompareOp) -> Result<(), Error> {
        use CompareOp::*;
        self.b.push("(");
        if let IsNull(is_null) = op {
            self.b.qualified(alias, column);
            self.b
                .push(if *is_null { " IS NULL" } else { " IS NOT NULL" });
            self.b.push(")");
            return Ok(());
        }
        self.b.qualified(alias, column);
        let (sql_op, v, suffix) = match op {
            Eq(v) => (" = ", v, ""),
            Neq(v) => (" <> ", v, ""),
            Gt(v) => (" > ", v, ""),
            Lt(v) => (" < ", v, ""),
            Gte(v) => (" >= ", v, ""),
            Lte(v) => (" <= ", v, ""),
            In(v) => (" = ANY(", v, ")"),
            Nin(v) => (" <> ALL(", v, ")"),
            Like(v) => (" LIKE ", v, ""),
            NLike(v) => (" NOT LIKE ", v, ""),
            ILike(v) => (" ILIKE ", v, ""),
            NILike(v) => (" NOT ILIKE ", v, ""),
            Similar(v) => (" SIMILAR TO ", v, ""),
            NSimilar(v) => (" NOT SIMILAR TO ", v, ""),
            Regex(v) => (" ~ ", v, ""),
            NRegex(v) => (" !~ ", v, ""),
            IRegex(v) => (" ~* ", v, ""),
            NIRegex(v) => (" !~* ", v, ""),
            Contains(v) => (" @> ", v, ""),
            ContainedIn(v) => (" <@ ", v, ""),
            HasKey(v) => (" ? ", v, ""),
            HasKeysAny(v) => (" ?| ", v, ""),
            HasKeysAll(v) => (" ?& ", v, ""),
            IsNull(_) => unreachable!("handled above"),
        };
        self.b.push(sql_op);
        self.value(v)?;
        self.b.push(suffix);
        self.b.push(")");
        Ok(())
    }
}

/// A Postgres array literal of text elements: `{"a","b"}`.
pub(crate) fn array_literal(items: &[String]) -> String {
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for c in item.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
    out.push('}');
    out
}
