//! Hasura webhook request/response transforms (`request_transform` / `response_transform`),
//! shared by actions, event triggers and cron triggers.
//!
//! This crate is IO-free. It parses the metadata JSON ([`RequestTransform::from_json`],
//! [`ResponseTransform::from_json`]), parse-checks templates as the metadata API does
//! ([`validate_transforms`]), and applies transforms to a transport-independent
//! [`Request`] and to raw response bodies. Templates are Kriti (`hge_kriti`); evaluation
//! errors are reported as V2's error bundle, a JSON array of error objects.
//!
//! ```
//! use hge_transforms::{Request, RequestContext, RequestTransform, apply_request_transform};
//! use serde_json::json;
//!
//! let t = RequestTransform::from_json(&json!({
//!     "version": 2,
//!     "method": "PUT",
//!     "url": "{{$base_url}}/users/{{$body.input.id}}",
//!     "body": {"action": "transform", "template": "{\"name\": {{$body.input.name}}}"}
//! }))
//! .unwrap();
//! let body = json!({"input": {"id": 7, "name": "Ada"}}).to_string().into_bytes();
//! let req = Request::new("POST", "http://api", vec![], body).unwrap();
//! let ctx = RequestContext::new(&req, Some("http://api"), None);
//! let out = apply_request_transform(&t, req, ctx).unwrap();
//! assert_eq!((out.method.as_str(), out.full_url()), ("PUT", "http://api/users/7".into()));
//! assert_eq!(out.body, br#"{"name":"Ada"}"#);
//! ```

mod apply;
mod http;
mod metadata;
mod test_webhook;

pub use apply::{
    ErrorBundle, RequestContext, ResponseContext, apply_request_transform,
    apply_response_transform, transformation_error,
};
pub use http::{Request, is_absolute_uri, parse_query, query_json, render_query};
pub use metadata::{
    BodyTransform, HeadersTransform, ParseError, QueryParamsTransform, RequestTransform,
    ResponseTransform, Tmpl, Version, parse_optional, validate_request_transform,
    validate_response_transform, validate_transforms,
};
pub use test_webhook::test_webhook_transform;

#[cfg(test)]
mod tests;
