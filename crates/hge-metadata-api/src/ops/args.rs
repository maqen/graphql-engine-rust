//! Argument parsing shared by the commands, with aeson's messages and paths.

use hge_core::Code;
use hge_events::model::HeaderConf;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::api::json_kind;
use crate::response::{ApiError, ApiResult, aeson_message, parse_failed};

pub(crate) type Obj = Map<String, Value>;

pub(crate) fn object<'a>(v: &'a Value, path: &str) -> ApiResult<&'a Obj> {
    v.as_object().ok_or_else(|| {
        parse_failed(
            path,
            format!(
                "parsing Object failed, expected Object, but encountered {}",
                json_kind(v)
            ),
        )
    })
}

/// A present, non-null field.
pub(crate) fn field<'a>(obj: &'a Obj, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

pub(crate) fn missing(path: &str, key: &str) -> ApiError {
    parse_failed(path, format!("key \"{key}\" not found"))
}

pub(crate) fn text(obj: &Obj, key: &str, path: &str) -> ApiResult<Option<String>> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(parse_failed(
            &format!("{path}.{key}"),
            format!(
                "parsing Text failed, expected String, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

/// A required string field; aeson calls the type `ty` (`Text`, or `String` for names).
pub(crate) fn required_string(obj: &Obj, key: &str, path: &str, ty: &str) -> ApiResult<String> {
    match obj.get(key) {
        None => Err(missing(path, key)),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v) => Err(parse_failed(
            &format!("{path}.{key}"),
            format!(
                "parsing {ty} failed, expected String, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

pub(crate) fn required_text(obj: &Obj, key: &str, path: &str) -> ApiResult<String> {
    required_string(obj, key, path, "Text")
}

pub(crate) fn boolean(obj: &Obj, key: &str, path: &str) -> ApiResult<Option<bool>> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(v) => Err(parse_failed(
            &format!("{path}.{key}"),
            format!(
                "parsing Bool failed, expected Boolean, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

pub(crate) fn int(obj: &Obj, key: &str, path: &str) -> ApiResult<Option<i64>> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::Number(n)) if n.is_i64() => Ok(n.as_i64()),
        // aeson's bounded `Int` accepts integral floats (`1.0`).
        Some(Value::Number(n))
            if n.as_f64().is_some_and(|f| {
                f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64
            }) =>
        {
            Ok(n.as_f64().map(|f| f as i64))
        }
        Some(v @ Value::Number(_)) => Err(parse_failed(
            &format!("{path}.{key}"),
            format!(
                "parsing Int failed, value is either floating or will cause over or underflow {v}"
            ),
        )),
        Some(v) => Err(parse_failed(
            &format!("{path}.{key}"),
            format!(
                "parsing Int failed, expected Number, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

pub(crate) fn typed<T: DeserializeOwned>(v: &Value, path: &str) -> ApiResult<T> {
    serde_json::from_value(v.clone()).map_err(|e| parse_failed(path, aeson_message(&e)))
}

/// `headers`: a list of `HeaderConf`, stored as decoded (unknown keys are dropped, as in V2).
pub(crate) fn headers(obj: &Obj, path: &str) -> ApiResult<Option<Vec<Value>>> {
    match field(obj, "headers") {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let header: HeaderConf = typed(h, &format!("{path}.headers[{i}]"))?;
                Ok(serde_json::to_value(header).expect("serialisable"))
            })
            .collect::<ApiResult<Vec<_>>>()
            .map(Some),
        Some(v) => Err(parse_failed(
            &format!("{path}.headers"),
            format!(
                "parsing [] failed, expected Array, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

pub(crate) fn db_error(prefix: &str, e: String) -> ApiError {
    ApiError::new(
        500,
        Code::Unexpected,
        format!("{prefix}: database query error"),
    )
    .with_internal(json!({ "error": { "message": e } }))
}
