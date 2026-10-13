//! Examples from the spec (§10, §12) and V2's transform tests.

use super::*;
use hge_core::Code;
use serde_json::{Value, json};

fn req_transform(v: Value) -> RequestTransform {
    RequestTransform::from_json(&v).unwrap()
}

fn action_request(url: &str, body: Value) -> Request {
    Request::new(
        "POST",
        url,
        vec![
            ("Content-Type".into(), "application/json".into()),
            ("User-Agent".into(), "hasura-graphql-engine/v2.51.0".into()),
        ],
        body.to_string().into_bytes(),
    )
    .unwrap()
}

fn apply(t: &RequestTransform, url: &str, body: Value) -> Result<Request, Vec<Value>> {
    let req = action_request(url, body);
    let ctx = RequestContext::new(
        &req,
        Some(url),
        Some(json!({"x-hasura-role": "user", "x-hasura-user-id": "42"})),
    );
    apply_request_transform(t, req, ctx).map_err(|b| b.0)
}

fn header<'a>(r: &'a Request, name: &str) -> Vec<&'a str> {
    r.headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

#[test]
fn defaults_and_round_trip() {
    let t = req_transform(json!({"body": "{{ $body }}"}));
    assert_eq!(t.version, Version::V1);
    assert_eq!(
        t.to_json(),
        json!({"version": 1, "template_engine": "Kriti", "body": "{{ $body }}"})
    );
    let v2 = json!({
        "version": 2,
        "template_engine": "Kriti",
        "method": "put",
        "url": "{{$base_url}}/x",
        "query_params": {"a": "{{$body.input.a}}", "flag": null},
        "request_headers": {"add_headers": {"X-A": "b"}, "remove_headers": ["content-type"]},
        "body": {"action": "x_www_form_urlencoded", "form_template": {"k": "v"}}
    });
    let t = req_transform(v2.clone());
    assert_eq!(t.to_json(), v2);
    assert_eq!(RequestTransform::from_json(&t.to_json()).unwrap(), t);
    // request_headers always carries both lists.
    let t = req_transform(json!({"request_headers": {}, "query_params": "a=1"}));
    assert_eq!(
        t.to_json(),
        json!({"version": 1, "template_engine": "Kriti", "query_params": "a=1",
            "request_headers": {"add_headers": {}, "remove_headers": []}})
    );
    // v1 can only represent a transform body; a v2 remove body survives.
    let t = req_transform(json!({"version": 2, "body": {"action": "remove"}}));
    assert_eq!(
        t.to_json(),
        json!({"version": 2, "template_engine": "Kriti", "body": {"action": "remove"}})
    );

    let r = ResponseTransform::from_json(&json!({"body": "{{ $body.x }}"})).unwrap();
    assert_eq!(
        r.to_json(),
        json!({"template_engine": "Kriti", "version": 1, "body": "{{ $body.x }}"})
    );
    let r2 = json!({"version": 2, "template_engine": "Kriti",
        "body": {"action": "transform", "template": "{{ $body }}"}});
    assert_eq!(ResponseTransform::from_json(&r2).unwrap().to_json(), r2);
    assert_eq!(
        ResponseTransform::from_json(&json!({})).unwrap().to_json(),
        json!({"template_engine": "Kriti", "version": 1})
    );
}

#[test]
fn parse_errors() {
    let e = |v: Value| RequestTransform::from_json(&v).unwrap_err().to_string();
    assert_eq!(
        e(json!({"version": 3})),
        "Error in $.version: expected 1 or 2, encountered 3"
    );
    assert_eq!(
        e(json!({"version": 2, "body": {"action": "nope"}})),
        "Error in $.body: invalid transform action"
    );
    assert_eq!(
        e(json!({"query_params": 1})),
        "Error in $.query_params: Invalid query parameter"
    );
    assert_eq!(
        e(json!({"version": 2, "body": {"action": "transform"}})),
        "Error in $.body: key \"template\" not found"
    );
    assert!(RequestTransform::from_json(&json!({"template_engine": "Go"})).is_err());
    assert!(RequestTransform::from_json(&json!({"body": {"action": "remove"}})).is_err());
    assert!(ResponseTransform::from_json(&json!({"version": 0})).is_err());
}

#[test]
fn validation_collects_kriti_parse_errors() {
    // Templates are parsed when read, but only reported by validation / at call time.
    let t = req_transform(json!({
        "url": "{{$base_url}}/{{ $body.x",
        "body": "{{ $body.hello }}",
        "request_headers": {"add_headers": {"X-A": "{{ ! }}"}}
    }));
    let e = validate_request_transform(&t).unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            Code::ValidationFailed,
            "request transform validation failed"
        )
    );
    let internal = e.internal.unwrap();
    let errors = internal.as_array().unwrap();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0]["error_code"], "Parse Error");
    assert_eq!(errors[1]["error_code"], "Lex Error");

    let r = ResponseTransform::from_json(&json!({"body": "{{ $body.x"})).unwrap();
    let e = validate_transforms(Some(&t), Some(&r)).unwrap_err();
    assert_eq!(e.message, "response transform validation failed");
    assert!(validate_transforms(Some(&req_transform(json!({"url": "http://x"}))), None).is_ok());
}

#[test]
fn method_url_body_and_headers() {
    let t = req_transform(json!({
        "version": 2,
        "method": "PUT",
        "url": "{{$base_url}}/users/{{$body.input.id}}",
        "request_headers": {
            "add_headers": {"Authorization": "Bearer {{$session_variables['x-hasura-user-id']}}"},
            "remove_headers": ["USER-AGENT"]
        },
        "body": {"action": "transform", "template":
            "{ \"input\": { \"arg\": { \"id\": {{ $body.input.arg.name }}, \"name\": {{ $body.input.arg.id }} } } }"}
    }));
    let out = apply(
        &t,
        "http://localhost:3000/api",
        json!({"input": {"id": 5, "arg": {"id": 1, "name": "n"}}}),
    )
    .unwrap();
    assert_eq!(out.method, "PUT");
    assert_eq!(out.full_url(), "http://localhost:3000/api/users/5");
    assert_eq!(
        String::from_utf8(out.body).unwrap(),
        r#"{"input":{"arg":{"id":"n","name":1}}}"#
    );
    assert_eq!(
        out.headers,
        vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Authorization".to_owned(), "Bearer 42".to_owned()),
        ]
    );
}

#[test]
fn add_headers_appends_without_replacing() {
    let t =
        req_transform(json!({"request_headers": {"add_headers": {"content-type": "text/plain"}}}));
    let out = apply(&t, "http://h", json!({})).unwrap();
    assert_eq!(
        header(&out, "content-type"),
        vec!["application/json", "text/plain"]
    );
}

#[test]
fn url_query_string_is_overwritten_by_query_params() {
    // A URL transform's own query string is lost; the original one is kept.
    let t = req_transform(json!({"url": "{{$base_url}}/x?a=1"}));
    let out = apply(&t, "http://h/base", json!({})).unwrap();
    assert_eq!(out.full_url(), "http://h/base/x");
    let t = req_transform(json!({"url": "https://other:8443/x?a=1"}));
    let out = apply(&t, "http://h/base?orig=1", json!({})).unwrap();
    assert_eq!(out.full_url(), "https://other:8443/x?orig=1");
    // With query_params, they replace everything.
    let t = req_transform(json!({"url": "http://www.google.com", "query_params": {"foo": "bar"}}));
    let out = apply(&t, "http://localhost:1234?orig=1", json!({})).unwrap();
    assert_eq!(out.full_url(), "http://www.google.com?foo=bar");
}

#[test]
fn query_params_object_and_string() {
    let t = req_transform(json!({"query_params": {
        "b": "{{$body.input.b}}",
        "a": "{{$body.input.missing?}}",
        "{{$body.input.k}}": null,
        "flag": null
    }}));
    let out = apply(
        &t,
        "http://h/p?x=1",
        json!({"input": {"b": "x y", "k": "key"}}),
    )
    .unwrap();
    assert_eq!(out.full_url(), "http://h/p?b=x+y&flag&key");

    // lib/api-tests WebhookTransformSpec: a string template built with range/concat.
    let t = req_transform(json!({"query_params":
        "{{ concat([\"tags=\", concat({{ range _, x := $body.input }} \"{{x}},\" {{ end }})]) }}"}));
    let out = apply(&t, "http://h", json!({"input": ["apple", "banana"]})).unwrap();
    assert_eq!(out.full_url(), "http://h/?tags=apple%2Cbanana%2C");
    assert_eq!(
        out.query,
        vec![("tags".into(), Some("apple,banana,".into()))]
    );
}

#[test]
fn query_params_context_comes_from_the_handler_url() {
    let t = req_transform(json!({"body": "{{ $query_params }}"}));
    let out = apply(&t, "http://h/p?a=1&b&c=x%20y", json!({})).unwrap();
    assert_eq!(
        String::from_utf8(out.body).unwrap(),
        r#"[["a","1"],["b",null],["c","x y"]]"#
    );
}

#[test]
fn form_urlencoded_and_remove_bodies() {
    let t = req_transform(json!({"version": 2, "body": {
        "action": "x_www_form_urlencoded",
        "form_template": {"foo": "{{$body.input.foo}}", "baz": "world", "gone": "{{$body.nope?}}"}
    }}));
    let out = apply(&t, "http://h", json!({"input": {"foo": "bar"}})).unwrap();
    assert_eq!(out.body, b"baz=world&foo=bar");
    // Content-Type is not changed.
    assert_eq!(header(&out, "content-type"), vec!["application/json"]);

    let t = req_transform(
        json!({"version": 2, "body": {"action": "x_www_form_urlencoded",
        "form_template": {"q": "a b&c=d/é"}}}),
    );
    let out = apply(&t, "http://h", json!({})).unwrap();
    assert_eq!(
        String::from_utf8(out.body).unwrap(),
        "q=a%20b%26c%3Dd%2F%C3%A9"
    );

    let t = req_transform(json!({"version": 2, "body": {"action": "remove"}}));
    assert!(apply(&t, "http://h", json!({})).unwrap().body.is_empty());
}

#[test]
fn errors_are_collected_into_one_bundle() {
    let t = req_transform(json!({
        "url": "{{$body.nope}}",
        "body": "{{ $body.world }}",
        "request_headers": {"add_headers": {"X-A": "{{$x}}", "X-B": "{{ getSessionVariable(\"x-hasura-missing\") }}"}}
    }));
    let bundle = apply(&t, "http://h", json!({"hello": 1})).unwrap_err();
    assert_eq!(bundle.len(), 4);
    assert_eq!(
        bundle[1],
        json!({"error_code": "Attribute Error", "message": "'Object' has no attritubte 'world'.",
            "source_position": {"start_line": 0, "start_column": 4, "end_line": 0, "end_column": 15}})
    );
    assert_eq!(bundle[2]["message"], "Variable '$x' not in scope");
    assert_eq!(bundle[3]["error_code"], "Function Error");
    assert_eq!(
        bundle[3]["message"],
        "Session variable \"x-hasura-missing\" not found"
    );

    let t = req_transform(json!({"url": "not a url"}));
    assert_eq!(
        apply(&t, "http://h", json!({})).unwrap_err(),
        vec![json!({"error_code": "TransformationError", "message": "Invalid URL: not a url"})]
    );
    // A template that failed to parse when read fails at call time.
    let t = req_transform(json!({"body": "{{ $body"}));
    assert_eq!(
        apply(&t, "http://h", json!({})).unwrap_err()[0]["error_code"],
        "Parse Error"
    );
}

#[test]
fn response_transforms() {
    let resp = |t: Value, body: &[u8], request: Value| {
        let t = ResponseTransform::from_json(&t).unwrap();
        let ctx = ResponseContext {
            request,
            status: 201,
            session_variables: json!({"x-hasura-role": "user"}),
        };
        apply_response_transform(&t, body, ctx)
            .map(|v| v.to_string())
            .map_err(|b| b.0)
    };
    // results_list_transformed_output_success: range over the body.
    assert_eq!(
        resp(
            json!({"version": 2, "body": {"action": "transform", "template":
                "{{ range _, x := $body.results }} {\"id\": {{x.k}}} {{ end }}"}}),
            br#"{"results": [{"k": 1}, {"k": 2}]}"#,
            Value::Null
        )
        .unwrap(),
        r#"[{"id":1},{"id":2}]"#
    );
    assert_eq!(
        resp(
            json!({"body": "[{{$response.status}}, {{$request}}, {{$session_variables['x-hasura-role']}}, {{$body}}]"}),
            b"not json",
            Value::Null
        )
        .unwrap(),
        r#"[201,null,"user",null]"#
    );
    assert_eq!(
        resp(
            json!({"version": 2, "body": {"action": "remove"}}),
            b"{}",
            Value::Null
        )
        .unwrap(),
        "null"
    );
    assert_eq!(
        resp(
            json!({"version": 2, "body": {"action": "x_www_form_urlencoded", "form_template": {"a": "{{$body.a}}"}}}),
            br#"{"a": "x y"}"#,
            Value::Null
        )
        .unwrap(),
        r#""a=x%20y""#
    );
    // Without a body transform the body is re-encoded; invalid JSON becomes null.
    assert_eq!(
        resp(json!({"version": 2}), b"oops", Value::Null).unwrap(),
        "null"
    );
    assert_eq!(
        resp(json!({"version": 2}), br#"{"b": 1, "a": 2}"#, Value::Null).unwrap(),
        r#"{"a":2,"b":1}"#
    );
    let e = resp(json!({"body": "{{ $body.x }}"}), b"[]", Value::Null).unwrap_err();
    assert_eq!(e[0]["error_code"], "Type Error");
}

#[test]
fn response_request_context_repeats_the_transformed_request() {
    let req = action_request("http://h/p?a=1", json!({"x": 1}));
    let ctx = RequestContext::new(&req, Some("http://h/p?a=1"), None);
    let c = json!({"base_url": "http://h/p?a=1", "body": {"x": 1}, "query_params": [["a", "1"]],
        "session_variables": null});
    assert_eq!(
        ctx.to_response_json(),
        json!({"body": c, "method": c, "query_params": c, "request_headers": c, "url": c})
    );
}

#[test]
fn test_webhook_transform_success() {
    let out = test_webhook_transform(&json!({
        "webhook_url": "http://localhost:1234",
        "body": {"hello": "world"},
        "request_headers": [["Content-Type", "application/json"]],
        "request_transform": {
            "body": "{{ $body }}",
            "url": "http://www.google.com",
            "template_engine": "Kriti",
            "method": "POST",
            "query_params": {"foo": "bar"},
            "request_headers": {"add_headers": {"foo": "bar"}, "remove_headers": []}
        }
    }))
    .unwrap();
    assert_eq!(
        out,
        json!({
            "webhook_url": "http://www.google.com?foo=bar",
            "method": "POST",
            "headers": [["content-type", "application/json"], ["foo", "bar"]],
            "body": {"hello": "world"}
        })
    );

    // Defaults: GET, the URL path is `/`, env templates come from the mock env.
    let out = test_webhook_transform(&json!({
        "webhook_url": "{{BASE}}/x",
        "env": {"BASE": "http://example.com"},
        "body": null,
        "session_variables": {"x-hasura-user-id": "1"},
        "request_transform": {"version": 2, "url": "{{$base_url}}/{{$session_variables['x-hasura-user-id']}}",
            "body": {"action": "remove"}}
    }))
    .unwrap();
    assert_eq!(
        out,
        json!({"webhook_url": "http://example.com/x/1", "method": "GET", "headers": [], "body": ""})
    );
    let out = test_webhook_transform(&json!({
        "webhook_url": {"from_env": "HOOK"},
        "env": {"HOOK": "http://localhost:1234"},
        "body": {},
        "request_transform": {"version": 2, "body": {"action": "x_www_form_urlencoded",
            "form_template": {"foo": "bar", "baz": "world"}}}
    }))
    .unwrap();
    assert_eq!(out["webhook_url"], "http://localhost:1234/");
    assert_eq!(out["body"], "baz=world&foo=bar");
}

#[test]
fn test_webhook_transform_errors() {
    let run = |args: Value| test_webhook_transform(&args).unwrap_err();
    let e = run(json!({
        "webhook_url": "http://localhost:1234",
        "body": {"hello": "world"},
        "request_transform": {"body": "{{ $body.world }}"}
    }));
    assert_eq!(
        e.to_api_json(true),
        json!({
            "path": "$.args",
            "error": "request transform validation failed",
            "code": "validation-failed",
            "internal": [{"error_code": "Attribute Error", "message": "'Object' has no attritubte 'world'.",
                "source_position": {"start_line": 0, "start_column": 4, "end_line": 0, "end_column": 15}}]
        })
    );
    let e = run(json!({
        "webhook_url": "http://localhost:1234",
        "body": {},
        "request_transform": {"body": "{{ $body.hello }"}
    }));
    assert_eq!(e.message, "request transform validation failed");
    assert_eq!(e.internal.unwrap()[0]["error_code"], "Parse Error");

    let e = run(json!({"webhook_url": "{{NOPE}}/x", "body": {}, "request_transform": {}}));
    assert_eq!((e.code, e.path.as_str()), (Code::NotFound, "$.args"));
    assert!(
        e.message
            .starts_with("Missing Env Var: NOPE. For security reasons")
    );
    let e = run(json!({"webhook_url": "", "body": {}, "request_transform": {}}));
    assert_eq!(
        (e.code, e.message.as_str()),
        (Code::ParseFailed, "Invalid Url Template")
    );
    let e = run(json!({"webhook_url": {"from_env": "X"}, "body": {}, "request_transform": {}}));
    assert_eq!(e.message, "Missing Env Var");
    let e = run(json!({"webhook_url": "localhost:1234", "body": {}, "request_transform": {}}));
    assert_eq!(e.message, "request transform validation failed");
    assert_eq!(
        e.internal.unwrap()[0]["error_code"],
        "Request Initialization Error"
    );
    let e = run(json!({"webhook_url": "http://h", "body": {},
        "request_transform": {"version": 3}}));
    assert_eq!(
        (e.code, e.path.as_str(), e.message.as_str()),
        (
            Code::ParseFailed,
            "$.args.request_transform.version",
            "expected 1 or 2, encountered 3"
        )
    );
}

#[test]
fn url_transform_cannot_switch_host_via_userinfo() {
    for bad in [
        "http://h@evil.com/",
        "http://user:pass@evil.com/x",
        "http://h:70000/x",
    ] {
        let t = req_transform(json!({"url": bad}));
        assert_eq!(
            apply(&t, "http://h", json!({})).unwrap_err(),
            vec![
                json!({"error_code": "TransformationError", "message": format!("Invalid URL: {bad}")})
            ],
            "{bad}"
        );
    }
    // Userinfo in the handler URL itself is basic auth, which remove_headers can drop.
    let t = req_transform(json!({"method": "PUT"}));
    let out = apply(&t, "http://u:p@h/x", json!({})).unwrap();
    assert_eq!(header(&out, "authorization"), vec!["Basic dTpw"]);
    assert_eq!(out.full_url(), "http://h/x");
    // `$base_url` keeps the credentials, so a URL built from it is refused too (V2 would
    // try to resolve the host `u:p@h` and fail).
    let t = req_transform(json!({"url": "{{$base_url}}/y"}));
    assert_eq!(
        apply(&t, "http://u:p@h", json!({})).unwrap_err()[0]["message"],
        "Invalid URL: http://u:p@h/y"
    );
    let t = req_transform(json!({"request_headers": {"remove_headers": ["Authorization"]}}));
    assert!(
        header(
            &apply(&t, "http://u:p@h", json!({})).unwrap(),
            "authorization"
        )
        .is_empty()
    );
}

#[test]
fn unsendable_methods_and_headers_are_transformation_errors() {
    let t = req_transform(json!({
        "method": "GE T",
        "request_headers": {"add_headers": {
            "X-Ok": "fine",
            "X-Injected": "a\\r\\nX-Evil: 1",
            "Bad Name": "v"
        }}
    }));
    let bundle = apply(&t, "http://h", json!({})).unwrap_err();
    assert_eq!(
        bundle,
        vec![
            json!({"error_code": "TransformationError", "message": "Invalid HTTP method", "value": "GE T"}),
            json!({"error_code": "TransformationError", "message": "Invalid header: X-Injected", "value": "a\r\nX-Evil: 1"}),
            json!({"error_code": "TransformationError", "message": "Invalid header: Bad Name", "value": "v"}),
        ]
    );
}

#[test]
fn version_accepts_integral_floats() {
    assert_eq!(
        req_transform(json!({"version": 2.0, "body": {"action": "remove"}})).version,
        Version::V2
    );
    assert!(RequestTransform::from_json(&json!({"version": 2.5})).is_err());
}

#[test]
fn outputs_have_sorted_keys() {
    // The body transform result is encoded with sorted keys, even for interpolated input.
    let t = req_transform(json!({"body": "{\"z\": 1, \"a\": {{ $body }}}"}));
    let out = apply(
        &t,
        "http://h",
        serde_json::from_str(r#"{"y": {"d": 1, "c": 2}, "b": 3}"#).unwrap(),
    )
    .unwrap();
    assert_eq!(out.body, br#"{"a":{"b":3,"y":{"c":2,"d":1}},"z":1}"#);
    // to_json sorts the HashMap-backed objects.
    let t = req_transform(json!({
        "version": 2,
        "query_params": {"b": "1", "a": null},
        "request_headers": {"add_headers": {"X-B": "1", "X-A": "2"}},
        "body": {"action": "x_www_form_urlencoded", "form_template": {"z": "1", "y": "2"}}
    }));
    assert_eq!(
        t.to_json().to_string(),
        r#"{"version":2,"template_engine":"Kriti","query_params":{"a":null,"b":"1"},"request_headers":{"add_headers":{"X-A":"2","X-B":"1"},"remove_headers":[]},"body":{"action":"x_www_form_urlencoded","form_template":{"y":"2","z":"1"}}}"#
    );
    // Query params are ordered by their evaluated key.
    let t = req_transform(json!({"query_params": {"a": "1", "{{$body.k}}": "2"}}));
    let out = apply(&t, "http://h", json!({"k": "0"})).unwrap();
    assert_eq!(out.full_url(), "http://h/?0=2&a=1");
}
