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
            BoolExp::Compare { column, op, .. } => self.compare(alias, column, op, None)?,
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
            BoolExp::CompareComputed { call, op, .. } => {
                self.compare_with(op, |t| t.function_call(call, alias))?
            }
            BoolExp::ComputedTable { call, exp } => {
                let rel = self.fresh("_e");
                self.b.push("EXISTS (SELECT 1 FROM ");
                self.function_call(call, alias)?;
                self.b.push(" AS ").ident(&rel).push(" WHERE ");
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
            BoolExp::Redacted { condition, exp } => match &**exp {
                BoolExp::Compare { column, op, .. } => {
                    self.compare(alias, column, op, Some((condition, root)))?
                }
                other => {
                    return Err(Error::unexpected(format!(
                        "redaction applies to column comparisons only, not {other:?}"
                    )));
                }
            },
            BoolExp::AggregationPredicate(p) => self.aggregation_predicate(p, alias)?,
        }
        Ok(())
    }

    /// `redact`: read the column as `CASE WHEN condition THEN col ELSE NULL END` (condition
    /// rendered with the given root).
    fn compare(
        &mut self,
        alias: &str,
        column: &str,
        op: &CompareOp,
        redact: Option<(&BoolExp, &str)>,
    ) -> Result<(), Error> {
        self.compare_with(op, |t| t.column(alias, column, redact))
    }

    /// `(<lhs> <op> <value>)`, with `lhs` rendered by the callback.
    fn compare_with(
        &mut self,
        op: &CompareOp,
        mut lhs: impl FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.compare_cast(op, &[], &mut lhs)
    }

    /// [`Self::compare_with`] with `lhs` cast to each of `casts`, innermost first.
    fn compare_cast(
        &mut self,
        op: &CompareOp,
        casts: &[&hge_core::ColumnType],
        lhs: &mut dyn FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        use CompareOp::*;
        match op {
            Cast { to, op } => {
                let mut casts = casts.to_vec();
                casts.push(to);
                return self.compare_cast(op, &casts, lhs);
            }
            Spatial(f, v) => {
                self.b.push(f.function()).push("(");
                self.operand(casts, lhs)?;
                self.b.push(", ");
                self.value(v)?;
                self.b.push(")");
                return Ok(());
            }
            DWithin(d) => {
                let hge_ir::DWithin {
                    three_d,
                    from,
                    distance,
                    use_spheroid,
                } = &**d;
                self.b.push(if *three_d {
                    "ST_3DDWithin("
                } else {
                    "ST_DWithin("
                });
                self.operand(casts, lhs)?;
                self.b.push(", ");
                self.value(from)?;
                self.b.push(", ");
                self.value(distance)?;
                if let Some(s) = use_spheroid {
                    self.b.push(", ");
                    self.value(s)?;
                }
                self.b.push(")");
                return Ok(());
            }
            _ => {}
        }
        self.b.push("(");
        self.operand(casts, lhs)?;
        if let IsNull(is_null) = op {
            self.b
                .push(if *is_null { " IS NULL" } else { " IS NOT NULL" });
            self.b.push(")");
            return Ok(());
        }
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
            IsNull(_) | Cast { .. } | Spatial(..) | DWithin(_) => {
                unreachable!("handled above")
            }
        };
        self.b.push(sql_op);
        self.value(v)?;
        self.b.push(suffix);
        self.b.push(")");
        Ok(())
    }

    /// The left-hand side of a comparison, cast in order.
    fn operand(
        &mut self,
        casts: &[&hge_core::ColumnType],
        lhs: &mut dyn FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for _ in casts {
            self.b.push("(");
        }
        lhs(self)?;
        for to in casts {
            self.b.push(")::");
            self.b.type_name(to);
        }
        Ok(())
    }
}

impl Translator<'_> {
    fn column(
        &mut self,
        alias: &str,
        column: &str,
        redact: Option<(&BoolExp, &str)>,
    ) -> Result<(), Error> {
        match redact {
            None => {
                self.b.qualified(alias, column);
            }
            Some((condition, root)) => {
                self.b.push("(CASE WHEN ");
                self.bool_exp_in(condition, alias, root)?;
                self.b.push(" THEN ");
                self.b.qualified(alias, column);
                self.b.push(" ELSE NULL END)");
            }
        }
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
