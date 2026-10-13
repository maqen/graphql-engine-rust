use hge_metadata::{EndpointMethod, Metadata};
use serde_json::{Value, json};

use super::*;

fn metadata(endpoints: Value) -> Metadata {
    hge_metadata::from_json(json!({
        "version": 3,
        "query_collections": [{"name": "c1", "definition": {"queries": [
            {"name": "ok", "query": "query ok($id: Int!) { author_by_pk(id: $id) { id } }"},
            {"name": "vars", "query": "query v($i: Int, $b: Boolean!, $s: String, $u: uuid!, $l: [Int!], $n: numeric) { a }"},
            {"name": "short", "query": "# c\n { author { id } }"},
            {"name": "two", "query": "query a { a } query b { b }"},
            {"name": "frag", "query": "query a { ...f } fragment f on query_root { a }"},
            {"name": "sub", "query": "subscription s { a }"},
        ]}}],
        "rest_endpoints": endpoints,
    }))
    .unwrap()
}

fn ep(name: &str, url: &str, methods: &[&str], query: &str) -> Value {
    json!({"name": name, "url": url, "methods": methods,
        "definition": {"query": {"collection_name": "c1", "query_name": query}}})
}

fn reasons(issues: &[hge_core::InconsistentObject]) -> Vec<String> {
    issues.iter().map(|o| o.display_reason()).collect()
}

#[test]
fn definition_checks_match_v2() {
    let md = metadata(json!([
        {"name": "nocol", "url": "a", "methods": ["GET"],
            "definition": {"query": {"collection_name": "zz", "query_name": "ok"}}},
        ep("noq", "b", &["GET"], "zz"),
        ep("short", "c", &["GET"], "short"),
        ep("two", "d", &["GET"], "two"),
        ep("frag", "e", &["GET"], "frag"),
        ep("sub", "f", &["GET"], "sub"),
        ep("ok", "g", &["GET"], "ok"),
    ]));
    let (endpoints, issues) = build(&md);
    assert_eq!(
        reasons(&issues),
        [
            "Inconsistent object: in endpoint nocol: collection with name zz does not exist",
            "Inconsistent object: in endpoint noq: query with name zz does not exist in collection c1",
            "Inconsistent object: in endpoint short: query with name short has multiple definitions.",
            "Inconsistent object: in endpoint two: query with name two has multiple definitions.",
            "Inconsistent object: in endpoint frag: query with name frag has multiple definitions.",
            "Inconsistent object: in endpoint sub: query with name sub is a subscription",
        ]
    );
    assert_eq!(issues[0].name, "rest_endpoint nocol");
    let get = RequestMethod::Endpoint(EndpointMethod::Get);
    assert_eq!(
        endpoints.lookup(get, "c").unwrap_err().message,
        "Endpoint not found"
    );
    assert_eq!(endpoints.lookup(get, "g").unwrap().0.name, "ok");
}

#[test]
fn path_checks_and_ambiguity_match_v2() {
    let md = metadata(json!([
        ep("e_dup", "g/:x/:x", &["GET"], "ok"),
        ep("e_colon", "h/:", &["GET"], "ok"),
        ep("e_ok", "author/:id", &["GET"], "ok"),
        ep("e_amb", "author/1", &["GET", "POST"], "ok"),
        ep("e_amb2", ":x/1", &["GET"], "ok"),
        ep("e_amb3", "zz/:y", &["PUT"], "ok"),
        ep("e_amb4", "zz/:q", &["PUT"], "ok"),
        ep("e_post", "zz/:q", &["POST"], "ok"),
    ]));
    let (endpoints, issues) = build(&md);
    assert_eq!(
        reasons(&issues),
        [
            "Duplicate variables found in endpoint path: g/:x/:x",
            "Empty segments or unnamed variables are not allowed: h/:",
            "Ambiguous URL paths: author/1, :x/1, h/:, author/:id",
            "Ambiguous URL paths: zz/:y, zz/:q",
        ]
    );
    let amb = issues[2].to_json();
    assert_eq!(amb["conflicts"][0]["name"], "e_amb");
    assert_eq!(
        amb["conflicts"][0]["definition"]["query"],
        "query ok($id: Int!) { author_by_pk(id: $id) { id } }"
    );
    assert!(issues[2].involves("rest_endpoint", "rest_endpoint e_ok"));
    assert!(!issues[2].involves("rest_endpoint", "rest_endpoint e_amb3"));

    let get = RequestMethod::Endpoint(EndpointMethod::Get);
    let err = |m, p| endpoints.lookup(m, p).unwrap_err();
    assert_eq!(
        err(get, "author/1").message,
        "Multiple endpoints match request"
    );
    assert_eq!(err(get, "h/1").message, "Multiple endpoints match request");
    let (e, vars) = endpoints.lookup(get, "author/2/").unwrap();
    assert_eq!(
        (e.name.as_str(), vars),
        ("e_ok", vec![("id".into(), "2".into())])
    );
    let del = RequestMethod::Endpoint(EndpointMethod::Delete);
    let e = err(del, "author/2");
    assert_eq!(
        (e.code, e.message.as_str()),
        (Code::MethodNotAllowed, "Allowed methods: GET")
    );
    assert_eq!(err(del, "zz/2").message, "Allowed methods: PUT, POST");
    assert_eq!(err(del, "author/1").message, "Allowed methods: GET, POST");
    // Empty segments are dropped.
    assert_eq!(endpoints.lookup(get, "author//2//").unwrap().0.name, "e_ok");
    assert_eq!(err(get, "nope").code, Code::NotFound);
    assert_eq!(
        err(RequestMethod::parse("HEAD"), "author/2").message,
        "Method HEAD not supported."
    );
    assert_eq!(
        err(RequestMethod::parse("FOO"), "author/2").message,
        "Nonstandard method not allowed for REST endpoints"
    );
}

#[test]
fn variables_follow_v2_precedence_and_typing() {
    let md = metadata(json!([ep("v", "v/:s", &["GET"], "vars")]));
    let (endpoints, issues) = build(&md);
    assert!(issues.is_empty());
    let get = RequestMethod::Endpoint(EndpointMethod::Get);
    let (e, path) = endpoints.lookup(get, "v/a%20b").unwrap();
    let q = parse_query_string("i=5&b=&u=1&n=1.50&s=query&s=ignored&bad-name=1&l");
    let body = json!({"u": "from-body", "s": 1});
    let vars = e.variables(&q, body.as_object(), &path);
    // `l` is a list given as text.
    assert_eq!(
        vars.unwrap_err().message,
        "List variables are not currently supported in URL or Query parameters. (Variable l, with value \"\")"
    );
    let q = parse_query_string("i=5&b=&u=1&n=1.50&s=query&i=x+y");
    let vars = e.variables(&q, body.as_object(), &path).unwrap();
    assert_eq!(
        Value::Object(vars),
        json!({"i": "x y", "b": true, "u": "from-body", "n": 1.50, "s": "a b"})
    );
    let q = parse_query_string("foo=1");
    assert_eq!(
        e.variables(&q, None, &path).unwrap_err().message,
        "Unexpected variable foo"
    );
    // Empty text leaves a nullable variable unset.
    let vars = e.variables(&parse_query_string("i="), None, &[]).unwrap();
    assert!(vars.is_empty());
    let vars = e
        .variables(&parse_query_string("b=false&i=3"), None, &[])
        .unwrap();
    assert_eq!(Value::Object(vars), json!({"b": false, "i": 3}));
}

#[test]
fn percent_decoding() {
    assert_eq!(
        parse_query_string("a=X%2520Y"),
        [("a".into(), "X%20Y".into())]
    );
    assert_eq!(
        variables::percent_decode("%7B%22a%22%7D+%zz", false),
        "{\"a\"}+%zz"
    );
    assert_eq!(variables::percent_decode("%+31%2", false), "%+31%2");
    assert_eq!(variables::percent_decode("%C3%A9", false), "é");
}
