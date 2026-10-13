//! Kriti errors, source spans and their Hasura JSON serialization.

use serde_json::{Map, Value};

/// A source position: `line` is 0-based, `column` is 1-based and counts Unicode characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// A source range with an exclusive end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

impl Span {
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// The smallest span covering both (kriti's `Semigroup Span`).
    pub fn join(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// The `error_code` of a Kriti error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    InvalidPath,
    Attribute,
    Name,
    Type,
    Index,
    Parse,
    Lex,
    Function,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidPath => "Invalid Path",
            ErrorCode::Attribute => "Attribute Error",
            ErrorCode::Name => "Name Error",
            ErrorCode::Type => "Type Error",
            ErrorCode::Index => "Index Error",
            ErrorCode::Parse => "Parse Error",
            ErrorCode::Lex => "Lex Error",
            ErrorCode::Function => "Function Error",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A lex, parse or evaluation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{error_code}: {message}")]
pub struct KritiError {
    pub error_code: ErrorCode,
    pub message: String,
    pub span: Span,
}

impl KritiError {
    pub(crate) fn new(error_code: ErrorCode, message: impl Into<String>, span: Span) -> Self {
        Self {
            error_code,
            message: message.into(),
            span,
        }
    }

    /// Serializes as Hasura does:
    /// `{"error_code","message","source_position":{"end_column","end_line","start_column","start_line"}}`.
    ///
    /// Keys are inserted in sorted order, matching aeson's encoding byte for byte.
    pub fn to_json(&self) -> Value {
        let mut pos = Map::new();
        pos.insert("end_column".into(), self.span.end.column.into());
        pos.insert("end_line".into(), self.span.end.line.into());
        pos.insert("start_column".into(), self.span.start.column.into());
        pos.insert("start_line".into(), self.span.start.line.into());
        let mut obj = Map::new();
        obj.insert("error_code".into(), self.error_code.as_str().into());
        obj.insert("message".into(), self.message.clone().into());
        obj.insert("source_position".into(), Value::Object(pos));
        Value::Object(obj)
    }
}
