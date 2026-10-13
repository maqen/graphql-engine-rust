//! Action commands. So far only `test_webhook_transform`.

use serde_json::Value;

use crate::response::{ApiError, ApiResult};

/// `test_webhook_transform` (spec 06 §12): apply a request transform to a sample request
/// and return it. Every failure is a 400, as in V2 (`err400`), whatever its code.
pub fn test_webhook_transform(args: &Value) -> ApiResult<Value> {
    hge_transforms::test_webhook_transform(args).map_err(|error| ApiError { status: 400, error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn errors_are_400() {
        let e = test_webhook_transform(&json!({
            "webhook_url": {"from_env": "NOPE"}, "body": {}, "request_transform": {}
        }))
        .unwrap_err();
        assert_eq!(
            (e.status, e.error.code, e.error.path.as_str()),
            (400, hge_core::Code::NotFound, "$.args")
        );
        let ok = test_webhook_transform(&json!({
            "webhook_url": "http://localhost:1234", "body": {"a": 1},
            "request_transform": {"method": "PUT"}
        }))
        .unwrap();
        assert_eq!(ok["method"], "PUT");
    }
}
