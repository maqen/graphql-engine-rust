//! A tiny SQL text builder: identifier quoting, string literals, and parameters.

use std::fmt::Write;

use hge_core::ColumnType;

/// SQL text plus its parameters (text format, `None` = NULL).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SqlBuilder {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// Multiplexed subscriptions: parameters are read from this JSON array expression (the
    /// cohort's variables) instead of being bound as `$n`.
    vars: Option<String>,
    /// `/v1/graphql/explain`: parameters are written as literals (`('<text>')`) instead.
    inline: bool,
}

impl SqlBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A builder whose parameter `n` (1-based) renders as `(<vars> ->> n-1)`, so one
    /// statement can serve every cohort of a multiplexed subscription.
    pub fn multiplexed(vars: &str) -> Self {
        SqlBuilder {
            vars: Some(vars.to_owned()),
            ..Self::default()
        }
    }

    /// A builder that writes every parameter as a literal, for `EXPLAIN` output that can be
    /// read and run on its own (V2 inlines values the same way).
    pub fn inline() -> Self {
        SqlBuilder {
            inline: true,
            ..Self::default()
        }
    }

    pub fn push(&mut self, s: &str) -> &mut Self {
        self.sql.push_str(s);
        self
    }

    pub fn push_char(&mut self, c: char) -> &mut Self {
        self.sql.push(c);
        self
    }

    pub fn ident(&mut self, name: &str) -> &mut Self {
        push_ident(&mut self.sql, name);
        self
    }

    /// `"schema"."name"[]…`
    pub fn type_name(&mut self, ty: &ColumnType) -> &mut Self {
        push_type(&mut self.sql, ty);
        self
    }

    /// `"alias"."column"`
    pub fn qualified(&mut self, alias: &str, column: &str) -> &mut Self {
        self.ident(alias).push_char('.').ident(column)
    }

    /// `'literal'` with quotes escaped; `E'…'` with backslashes doubled when it has any, so
    /// it reads the same whatever `standard_conforming_strings` is (like V2's `pgFmtLit`).
    pub fn string_literal(&mut self, s: &str) -> &mut Self {
        if s.contains('\\') {
            self.sql.push('E');
        }
        self.sql.push('\'');
        for c in s.chars() {
            if c == '\'' || c == '\\' {
                self.sql.push(c);
            }
            self.sql.push(c);
        }
        self.sql.push('\'');
        self
    }

    /// Bind a text-format parameter cast to `ty`: `$n::"schema"."type"`.
    pub fn param(&mut self, value: Option<String>, ty: &ColumnType) -> &mut Self {
        self.placeholder(value);
        self.sql.push_str("::");
        push_type(&mut self.sql, ty);
        self
    }

    /// Bind `value` and write its untyped placeholder: `($n)`, or `(<vars>->>n-1)` when
    /// multiplexed.
    fn placeholder(&mut self, value: Option<String>) {
        if self.inline {
            match value {
                None => self.sql.push_str("(NULL)"),
                Some(v) => {
                    self.sql.push('(');
                    self.string_literal(&v);
                    self.sql.push(')');
                }
            }
            return;
        }
        self.params.push(value);
        let n = self.params.len();
        match &self.vars {
            None => write!(self.sql, "(${n})"),
            Some(vars) => write!(self.sql, "({vars}->>{})", n - 1),
        }
        .expect("write to string");
    }

    /// Bind a GraphQL input value: like [`Self::param`], except that `geometry` /
    /// `geography` inputs are GeoJSON (`ST_GeomFromGeoJSON($n)::"type"`).
    pub fn input_param(&mut self, value: Option<String>, ty: &ColumnType) -> &mut Self {
        if ty.dimensions == 0 && matches!(ty.name.as_str(), "geometry" | "geography") {
            self.sql.push_str("ST_GeomFromGeoJSON(");
            self.placeholder(value);
            self.sql.push_str("::text)::");
            push_type(&mut self.sql, ty);
            return self;
        }
        self.param(value, ty)
    }
}

pub fn push_ident(out: &mut String, name: &str) {
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

/// `"schema"."name"[]…` for a column type.
pub fn push_type(out: &mut String, ty: &ColumnType) {
    push_ident(out, &ty.schema);
    out.push('.');
    push_ident(out, &ty.name);
    for _ in 0..ty.dimensions {
        out.push_str("[]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        let mut b = SqlBuilder::new();
        b.string_literal("it's").push(" ").string_literal(r"a\b'");
        assert_eq!(b.sql, r"'it''s' E'a\\b'''");
    }

    #[test]
    fn quoting() {
        let mut b = SqlBuilder::new();
        b.qualified("t", "we\"ird")
            .push(" = ")
            .string_literal("it's");
        assert_eq!(b.sql, r#""t"."we""ird" = 'it''s'"#);
        b.push(" AND ")
            .param(Some("1".into()), &ColumnType::builtin("int4").array_of());
        assert!(b.sql.ends_with(r#"($1)::"pg_catalog"."int4"[]"#));
    }

    #[test]
    fn multiplexed_params() {
        let mut b = SqlBuilder::multiplexed(r#""_subs"."vars""#);
        b.param(Some("1".into()), &ColumnType::builtin("int4"))
            .push(", ")
            .param(None, &ColumnType::builtin("text"));
        assert_eq!(
            b.sql,
            r#"("_subs"."vars"->>0)::"pg_catalog"."int4", ("_subs"."vars"->>1)::"pg_catalog"."text""#
        );
        assert_eq!(b.params, vec![Some("1".to_owned()), None]);
    }

    #[test]
    fn geometry_input_params() {
        let geometry = ColumnType {
            schema: "public".to_owned(),
            ..ColumnType::builtin("geometry")
        };
        let mut b = SqlBuilder::new();
        b.input_param(Some("{}".into()), &geometry);
        assert_eq!(
            b.sql,
            r#"ST_GeomFromGeoJSON(($1)::text)::"public"."geometry""#
        );
        let mut b = SqlBuilder::multiplexed(r#""_subs"."vars""#);
        b.input_param(Some("{}".into()), &geometry);
        assert_eq!(
            b.sql,
            r#"ST_GeomFromGeoJSON(("_subs"."vars"->>0)::text)::"public"."geometry""#
        );
    }
}
