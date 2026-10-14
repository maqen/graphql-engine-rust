//! `validate_input` (spec 04 §10): the engine runs in-process against a real Postgres and
//! calls a mock validation webhook served on a local port before each mutation.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hge_server::ServerConfig;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Requests the webhook received: `(path, headers, body)`.
type Calls = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

/// `/hook` rejects any input that mentions `"bad"`; `/broken` answers 400 without a
/// message; `/down` answers 500.
async fn start_webhook(calls: Calls) -> SocketAddr {
    async fn hook(
        State(calls): State<Calls>,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        let path = uri.path().to_owned();
        let bad = body["data"].to_string().contains("\"bad\"");
        calls.lock().unwrap().push((path.clone(), headers, body));
        match path.as_str() {
            "/hook" if bad => (
                StatusCode::BAD_REQUEST,
                Json(json!({"message": "titles cannot be bad"})),
            )
                .into_response(),
            "/broken" => (StatusCode::BAD_REQUEST, Json(json!({"error": 1}))).into_response(),
            "/down" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            _ => Json(json!({})).into_response(),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/{*path}", post(hook))
        .with_state(calls);
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

fn validate(path: &str) -> Value {
    json!({
        "type": "http",
        "definition": {
            "url": format!("{{{{HOOK_BASE}}}}{path}"),
            "headers": [
                {"name": "X-Secret", "value": "v"},
                {"name": "X-From-Env", "value_from_env": "HOOK_SECRET"},
            ],
            "timeout": 5,
        }
    })
}

fn metadata() -> Value {
    json!({
        "version": 3,
        "sources": [{
            "name": "default",
            "kind": "postgres",
            "configuration": {"connection_info": {"database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"}}},
            "tables": [
                {
                    "table": {"schema": "public", "name": "author"},
                    "array_relationships": [{
                        "name": "articles",
                        "using": {"foreign_key_constraint_on": {"table": "article", "column": "author_id"}},
                    }],
                    "select_permissions": [{"role": "user", "permission": {"columns": "*", "filter": {}}}],
                    "insert_permissions": [{"role": "user", "permission": {
                        "check": {}, "columns": ["name"],
                        "set": {"kind": "writer", "owner": "x-hasura-user-id"},
                        "validate_input": validate("/hook"),
                    }}],
                    "update_permissions": [{"role": "user", "permission": {
                        "columns": ["name"], "filter": {}, "validate_input": validate("/broken"),
                    }}],
                    "delete_permissions": [{"role": "user", "permission": {
                        "filter": {}, "validate_input": validate("/down"),
                    }}],
                },
                {
                    "table": {"schema": "public", "name": "article"},
                    "select_permissions": [{"role": "user", "permission": {"columns": "*", "filter": {}}}],
                    "insert_permissions": [{"role": "user", "permission": {
                        "check": {}, "columns": ["title", "author_id"],
                        "validate_input": validate("/hook"),
                    }}],
                },
            ],
        }],
    })
}

const SCHEMA: &str = "
    CREATE TABLE author (id serial PRIMARY KEY, name text NOT NULL, kind text, owner text);
    CREATE TABLE article (id serial PRIMARY KEY, author_id int REFERENCES author(id), title text);
";

#[tokio::test(flavor = "multi_thread")]
async fn validate_input_webhook() {
    let Some(db) = hge_testdb::TestDb::create("validate").await else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping validate_input tests");
        return;
    };
    let url = db.url().to_owned();
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client.batch_execute(SCHEMA).await.unwrap();

    let calls: Calls = Arc::default();
    let addr = start_webhook(calls.clone()).await;
    let engine = hge_server::build_engine(ServerConfig {
        database_url: Some(url.clone()),
        env_overrides: [
            ("HOOK_BASE".to_owned(), format!("http://{addr}")),
            ("HOOK_SECRET".to_owned(), "from-env".to_owned()),
            ("HASURA_GRAPHQL_DATABASE_URL".to_owned(), url.clone()),
        ]
        .into_iter()
        .collect(),
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let router = hge_server::routes::router(engine);
    let post = |path: &'static str, role: Option<&'static str>, body: Value| {
        let router = router.clone();
        async move {
            let mut req = Request::post(path).header("content-type", "application/json");
            if let Some(role) = role {
                req = req
                    .header("x-hasura-role", role)
                    .header("x-hasura-user-id", "42");
            }
            let resp = router
                .oneshot(req.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    let gql = |query: &str| json!({"query": query});
    let take = || std::mem::take(&mut *calls.lock().unwrap());

    let resp = post(
        "/v1/metadata",
        None,
        json!({"type": "replace_metadata", "args": metadata()}),
    )
    .await;
    assert_eq!(resp["message"], "success", "{resp}");

    // A nested insert: one call per table holding all its rows, parent first. Presets are
    // not sent; nested rows appear under the relationship.
    let resp = post(
        "/v1/graphql",
        Some("user"),
        gql(r#"mutation { insert_author(objects: [{name: "A", articles: {data: [{title: "t1"}]}}, {name: "Z", articles: {data: [{title: "t2"}]}}]) { affected_rows } }"#),
    )
    .await;
    assert_eq!(
        resp,
        json!({"data": {"insert_author": {"affected_rows": 4}}})
    );
    let seen = take();
    assert_eq!(seen.len(), 2, "{seen:?}");
    let (path, headers, body) = &seen[0];
    assert_eq!(path, "/hook");
    assert_eq!(headers["x-secret"], "v");
    assert_eq!(headers["x-from-env"], "from-env");
    assert_eq!(headers["content-type"], "application/json");
    assert!(
        headers["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("hasura-graphql-engine/")
    );
    assert_eq!(body["version"], 1);
    assert_eq!(body["role"], "user");
    assert_eq!(body["session_variables"]["x-hasura-user-id"], "42");
    assert_eq!(
        body["data"],
        json!({"input": [
            {"name": "A", "articles": {"data": [{"title": "t1"}]}},
            {"name": "Z", "articles": {"data": [{"title": "t2"}]}},
        ]})
    );
    assert_eq!(
        seen[1].2["data"],
        json!({"input": [{"title": "t1"}, {"title": "t2"}]})
    );

    // A 400 with a message rejects the mutation before it runs.
    let resp = post(
        "/v1/graphql",
        Some("user"),
        gql(r#"mutation { insert_author_one(object: {name: "B", articles: {data: [{title: "bad"}]}}) { id } }"#),
    )
    .await;
    assert_eq!(
        resp,
        json!({"errors": [{"message": "titles cannot be bad", "extensions": {"path": "$", "code": "validation-failed"}}]})
    );
    // The parent's call already sees the nested rows and stops the mutation.
    assert_eq!(take().len(), 1);

    // Every root field is validated before any runs: the first insert is not committed.
    let resp = post(
        "/v1/graphql",
        Some("user"),
        gql(r#"mutation { __typename ok: insert_author_one(object: {name: "D"}) { id } no: insert_article_one(object: {title: "bad"}) { id } }"#),
    )
    .await;
    assert_eq!(
        resp["errors"][0]["message"], "titles cannot be bad",
        "{resp}"
    );
    assert_eq!(take().len(), 2);
    let rows = client
        .query_one("SELECT count(*) FROM author", &[])
        .await
        .unwrap();
    assert_eq!(rows.get::<_, i64>(0), 2);

    // Updates send their arguments; a 400 without a string message is an unexpected error.
    let resp = post(
        "/v1/graphql",
        Some("user"),
        gql(r#"mutation { update_author_by_pk(pk_columns: {id: 1}, _set: {name: "C"}) { id } }"#),
    )
    .await;
    assert_eq!(
        resp["errors"][0]["message"],
        "received invalid response from input validation webhook"
    );
    assert_eq!(resp["errors"][0]["extensions"]["code"], "unexpected");
    let seen = take();
    assert_eq!(seen[0].0, "/broken");
    assert_eq!(
        seen[0].2["data"]["input"][0]["pk_columns"],
        json!({"id": 1})
    );
    assert_eq!(seen[0].2["data"]["input"][0]["_set"], json!({"name": "C"}));

    // Delete by primary key wraps its arguments in `pk_columns`; a 500 fails the mutation.
    let resp = post(
        "/v1/graphql",
        Some("user"),
        gql(r#"mutation { delete_author_by_pk(id: 1) { id } }"#),
    )
    .await;
    assert_eq!(
        resp["errors"][0]["message"],
        "internal error when validating input data"
    );
    let seen = take();
    assert_eq!(
        seen[0].2["data"],
        json!({"input": [{"pk_columns": {"id": 1}}]})
    );

    // Admin has no `validate_input`.
    let resp = post(
        "/v1/graphql",
        None,
        gql(r#"mutation { delete_article(where: {}) { affected_rows } }"#),
    )
    .await;
    assert!(resp.get("errors").is_none(), "{resp}");
    assert!(take().is_empty());
}
