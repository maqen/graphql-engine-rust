//! The Kriti AST (kriti's `ValueExt`).

use crate::error::Span;
use serde_json::Number;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Expr {
    pub span: Span,
    /// Boxed so `Expr` stays small: parser and evaluator frames hold many of them.
    pub kind: Box<ExprKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Gt,
    Lt,
    Gte,
    Lte,
    NotEq,
    Eq,
    And,
    Or,
    In,
    Default,
}

/// A path step: `.ident` or `[expr]` (`['lit']` is a string-literal expression).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Accessor {
    Key(String),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Elif {
    pub span: Span,
    pub cond: Expr,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExprKind {
    /// Fields sorted by key, duplicates removed (first occurrence wins).
    Object(Vec<(String, Expr)>),
    Array(Vec<Expr>),
    String(String),
    Number(Number),
    Bool(bool),
    Null,
    /// String template parts; literal fragments are `String` nodes.
    Template(Vec<Expr>),
    Var(String),
    Field(Expr, Accessor),
    /// `e?f1 f2 …`: errors in `e` become null, the whole chain is null-safe.
    Optional(Expr, Vec<Accessor>),
    If {
        cond: Expr,
        then: Expr,
        elifs: Vec<Elif>,
        otherwise: Expr,
    },
    Binary(BinOp, Expr, Expr),
    Range {
        index: Option<String>,
        binder: String,
        over: Expr,
        body: Expr,
    },
    Call(String, Expr),
}
