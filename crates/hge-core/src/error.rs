//! The engine-wide error type, shaped like Hasura V2's `QErr`.
//!
//! Every error that can reach a client carries a [`Code`] (serialised exactly like V2's
//! `code` strings), a human message, and a JSON path (`$.selectionSet.author.args.where`).

use std::fmt;

use serde_json::{Map, Value, json};

/// Hasura V2 error codes (`extensions.code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    AccessDenied,
    AlreadyExists,
    AlreadyTracked,
    BadRequest,
    Busy,
    Conflict,
    ConstraintError,
    ConstraintViolation,
    DataException,
    DependencyError,
    InvalidConfiguration,
    InvalidHeaders,
    InvalidJson,
    InvalidJwt,
    InvalidParams,
    MetadataWarnings,
    JwtInvalidClaims,
    JwtMissingRoleClaims,
    MethodNotAllowed,
    NotExists,
    NotFound,
    NotSupported,
    ParseFailed,
    PermissionDenied,
    PermissionError,
    PostgresError,
    RemoteSchemaError,
    Unexpected,
    UnexpectedPayload,
    ValidationFailed,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::AccessDenied => "access-denied",
            Code::AlreadyExists => "already-exists",
            Code::AlreadyTracked => "already-tracked",
            Code::BadRequest => "bad-request",
            Code::Busy => "busy",
            Code::Conflict => "conflict",
            Code::ConstraintError => "constraint-error",
            Code::ConstraintViolation => "constraint-violation",
            Code::DataException => "data-exception",
            Code::DependencyError => "dependency-error",
            Code::InvalidConfiguration => "invalid-configuration",
            Code::InvalidHeaders => "invalid-headers",
            Code::InvalidJson => "invalid-json",
            Code::InvalidJwt => "invalid-jwt",
            Code::InvalidParams => "invalid-params",
            Code::MetadataWarnings => "metadata-warnings",
            Code::JwtInvalidClaims => "jwt-invalid-claims",
            Code::JwtMissingRoleClaims => "jwt-missing-role-claims",
            Code::MethodNotAllowed => "method-not-allowed",
            Code::NotExists => "not-exists",
            Code::NotFound => "not-found",
            Code::NotSupported => "not-supported",
            Code::ParseFailed => "parse-failed",
            Code::PermissionDenied => "permission-denied",
            Code::PermissionError => "permission-error",
            Code::PostgresError => "postgres-error",
            Code::RemoteSchemaError => "remote-schema-error",
            Code::Unexpected => "unexpected",
            Code::UnexpectedPayload => "unexpected-payload",
            Code::ValidationFailed => "validation-failed",
        }
    }

    /// HTTP status V2 uses for this code on non-GraphQL endpoints (`/v1/metadata`, …).
    /// GraphQL errors on `/v1/graphql` are always returned with 200.
    pub fn http_status(self) -> u16 {
        match self {
            Code::AccessDenied | Code::InvalidJwt | Code::JwtInvalidClaims => 401,
            Code::InvalidHeaders | Code::JwtMissingRoleClaims => 400,
            Code::NotFound => 404,
            Code::Conflict => 409,
            Code::MethodNotAllowed => 405,
            Code::PermissionDenied => 403,
            Code::Unexpected | Code::PostgresError => 500,
            _ => 400,
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error that can be reported to a client.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: Code,
    pub message: String,
    /// JSON path of the offending input, V2 style (`$`, `$.selectionSet.author`).
    pub path: String,
    /// Extra details only shown in dev mode / to admins (`extensions.internal`).
    pub internal: Option<Box<Value>>,
    /// Replaces the whole `extensions` object of the GraphQL error (an action handler's
    /// `extensions`).
    pub extensions: Option<Box<Value>>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            path: "$".to_owned(),
            internal: None,
            extensions: None,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(Code::ValidationFailed, message)
    }

    pub fn unexpected(message: impl Into<String>) -> Self {
        Self::new(Code::Unexpected, message)
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn with_internal(mut self, internal: Value) -> Self {
        self.internal = Some(Box::new(internal));
        self
    }

    pub fn with_extensions(mut self, extensions: Value) -> Self {
        self.extensions = Some(Box::new(extensions));
        self
    }

    /// The GraphQL error object (`{"extensions": {...}, "message": ...}`).
    pub fn to_graphql_json(&self, include_internal: bool) -> Value {
        if let Some(ext) = &self.extensions {
            return json!({ "message": self.message, "extensions": ext });
        }
        let mut ext = Map::new();
        ext.insert("path".into(), Value::String(self.path.clone()));
        ext.insert("code".into(), Value::String(self.code.as_str().into()));
        if include_internal && let Some(internal) = &self.internal {
            ext.insert("internal".into(), (**internal).clone());
        }
        json!({ "message": self.message, "extensions": ext })
    }

    /// The error body used by the non-GraphQL APIs (`/v1/metadata`, `/v2/query`).
    pub fn to_api_json(&self, include_internal: bool) -> Value {
        // V2's key order: error, path, code (then internal).
        let mut obj = Map::new();
        obj.insert("error".into(), Value::String(self.message.clone()));
        obj.insert("path".into(), Value::String(self.path.clone()));
        obj.insert("code".into(), Value::String(self.code.as_str().into()));
        if include_internal && let Some(internal) = &self.internal {
            obj.insert("internal".into(), (**internal).clone());
        }
        Value::Object(obj)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.path, self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_error_shape() {
        let err = Error::validation("field 'x' not found in type: 'query_root'")
            .with_path("$.selectionSet.x");
        assert_eq!(
            err.to_graphql_json(false),
            json!({
                "extensions": {"path": "$.selectionSet.x", "code": "validation-failed"},
                "message": "field 'x' not found in type: 'query_root'"
            })
        );
    }
}
