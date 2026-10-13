//! Responses and errors of the metadata and schema APIs.

use hge_core::{Code, Error, InconsistentObject, inconsistency::inconsistencies_json};
use serde_json::{Value, json};

/// An HTTP response: status plus JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
}

impl ApiResponse {
    pub fn ok(body: Value) -> Self {
        ApiResponse { status: 200, body }
    }
}

/// An error with the HTTP status V2 sends for it (which does not always follow the code).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub error: Error,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        ApiError {
            status: error.code.http_status(),
            error,
        }
    }
}

impl ApiError {
    pub fn new(status: u16, code: Code, message: impl Into<String>) -> Self {
        ApiError {
            status,
            error: Error::new(code, message),
        }
    }

    /// A 400 with the given code.
    pub fn bad(code: Code, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.error.path = path.into();
        self
    }

    pub fn with_internal(mut self, internal: Value) -> Self {
        self.error.internal = Some(Box::new(internal));
        self
    }

    /// Nest the error's path under `prefix` (`$.foo` under `$.args[1]` → `$.args[1].foo`).
    pub fn under(mut self, prefix: &str) -> Self {
        let rest = self
            .error
            .path
            .strip_prefix('$')
            .unwrap_or(&self.error.path);
        self.error.path = format!("{prefix}{rest}");
        self
    }

    pub fn to_response(&self) -> ApiResponse {
        let mut body = self.error.to_api_json(true);
        // V2 renders `internal` from plain aeson values: keys sorted at every level.
        if let Some(internal) = body.get_mut("internal") {
            *internal = hge_metadata::export::sorted(internal.take());
        }
        ApiResponse {
            status: self.status,
            body,
        }
    }
}

pub fn success() -> Value {
    json!({ "message": "success" })
}

/// `400 parse-failed` at `path`.
pub fn parse_failed(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::bad(Code::ParseFailed, message).with_path(path)
}

pub fn not_exists(message: impl Into<String>) -> ApiError {
    ApiError::bad(Code::NotExists, message)
}

pub fn already_exists(message: impl Into<String>) -> ApiError {
    ApiError::bad(Code::AlreadyExists, message)
}

pub fn not_supported(message: impl Into<String>) -> ApiError {
    ApiError::bad(Code::NotSupported, message)
}

/// `{"is_consistent": …, "inconsistent_objects": […]}`
pub fn consistency(objects: &[InconsistentObject]) -> Value {
    json!({
        "inconsistent_objects": inconsistencies_json(objects),
        "is_consistent": objects.is_empty(),
    })
}

/// Parse command arguments with serde, reporting failures as V2 does.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: &Value) -> ApiResult<T> {
    serde_json::from_value(args.clone()).map_err(|e| parse_failed("$.args", aeson_message(&e)))
}

/// Rephrase a serde error the way aeson words it where the two differ in a fixed way.
pub fn aeson_message(e: &serde_json::Error) -> String {
    let msg = e.to_string();
    let msg = match msg.rfind(" at line ") {
        Some(i) => &msg[..i],
        None => &msg,
    };
    if let Some(rest) = msg.strip_prefix("missing field `")
        && let Some(field) = rest.strip_suffix('`')
    {
        return format!("key \"{field}\" not found");
    }
    msg.to_owned()
}
