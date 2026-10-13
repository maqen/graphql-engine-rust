//! A tiny SQL text builder: identifier quoting, string literals, and parameters.

use std::fmt::Write;

use hge_core::ColumnType;

/// SQL text plus its parameters (text format, `None` = NULL).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SqlBuilder {
    pub sql: String,
    pub params: Vec<Option<String>>,
}

impl SqlBuilder {
    pub fn new() -> Self {
        Self::default()
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

    /// `"alias"."column"`
    pub fn qualified(&mut self, alias: &str, column: &str) -> &mut Self {
        self.ident(alias).push_char('.').ident(column)
    }

    /// `'literal'` with quotes escaped.
    pub fn string_literal(&mut self, s: &str) -> &mut Self {
        self.sql.push('\'');
        for c in s.chars() {
            if c == '\'' {
                self.sql.push('\'');
            }
            self.sql.push(c);
        }
        self.sql.push('\'');
        self
    }

    /// Bind a text-format parameter cast to `ty`: `$n::"schema"."type"`.
    pub fn param(&mut self, value: Option<String>, ty: &ColumnType) -> &mut Self {
        self.params.push(value);
        let n = self.params.len();
        write!(self.sql, "(${n})::").expect("write to string");
        push_type(&mut self.sql, ty);
        self
    }

    /// Bind an untyped text parameter: `$n::text`.
    pub fn text_param(&mut self, value: impl Into<String>) -> &mut Self {
        self.params.push(Some(value.into()));
        let n = self.params.len();
        write!(self.sql, "(${n})::text").expect("write to string");
        self
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
}
