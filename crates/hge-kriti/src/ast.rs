//! The Kriti AST (kriti's `ValueExt`).

use crate::error::Span;
use serde_json::Number;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Expr {
    pub span: Span,
    pub kind: ExprKind,
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
    Expr(Box<Expr>),
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
    Field(Box<Expr>, Accessor),
    /// `e?f1 f2 …`: errors in `e` become null, the whole chain is null-safe.
    Optional(Box<Expr>, Vec<Accessor>),
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        elifs: Vec<Elif>,
        otherwise: Box<Expr>,
    },
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Range {
        index: Option<String>,
        binder: String,
        over: Box<Expr>,
        body: Box<Expr>,
    },
    Call(String, Box<Expr>),
}
