//! Remote schema tests against an in-process mock GraphQL server (built with
//! `async-graphql`).
//!
//! Cases live in `tests/remote_schema_cases/*.yaml`, in the same format as `tests/cases/`
//! plus `config.inconsistencies` (the expected `inconsistent_objects` names and reasons).
//! The mock's `lastRequest` field returns the body of the request it received before, so
//! cases can assert on what the engine forwarded. Needs `HASURA_GRAPHQL_DATABASE_URL`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use async_graphql::{
    Context, EmptySubscription, ID, Interface, Object, Request as GqlRequest, Schema, SimpleObject,
    Union,
};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use hge_server::ServerConfig;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/remote_schema_cases");

// --- mock remote server -----------------------------------------------------------------

static MOCK_URL: OnceLock<String> = OnceLock::new();
static LAST_REQUEST: Mutex<Option<String>> = Mutex::new(None);

/// The test's environment for URL and header templates.
fn test_env() -> std::collections::HashMap<String, String> {
    let url = MOCK_URL.get().expect("mock started").clone();
    [
        ("REMOTE_URL", url.clone()),
        ("REMOTE_GRAPHQL_URL", format!("{url}/graphql")),
        ("REMOTE_HEADER", "from-env".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

/// Request headers the mock saw (`x-*` and `authorization`), as `name: value`.
struct SeenHeaders(Vec<String>);

#[derive(SimpleObject, Clone)]
struct User {
    id: i32,
    name: String,
}

#[derive(SimpleObject, Clone)]
struct Human {
    id: ID,
    name: String,
    home_planet: Option<String>,
}

#[derive(SimpleObject, Clone)]
struct Droid {
    id: ID,
    name: String,
    primary_function: String,
}

#[derive(Interface, Clone)]
#[graphql(field(name = "id", ty = "&ID"), field(name = "name", ty = "&String"))]
enum Character {
    Human(Human),
    Droid(Droid),
}

#[derive(Union, Clone)]
enum SearchResult {
    User(User),
    Droid(Droid),
}

fn users() -> Vec<User> {
    vec![
        User {
            id: 1,
            name: "alice".into(),
        },
        User {
            id: 2,
            name: "bob".into(),
        },
    ]
}

fn characters() -> Vec<Character> {
    vec![
        Character::Human(Human {
            id: "1".into(),
            name: "Luke".into(),
            home_planet: Some("Tatooine".into()),
        }),
        Character::Droid(Droid {
            id: "2".into(),
            name: "R2-D2".into(),
            primary_function: "Astromech".into(),
        }),
    ]
}

#[derive(async_graphql::InputObject)]
struct Range {
    low: i32,
    high: i32,
}

#[derive(async_graphql::Enum, Copy, Clone, Eq, PartialEq)]
enum Order {
    Asc,
    Desc,
}

struct Query;

#[Object]
impl Query {
    async fn hello(&self) -> &str {
        "world"
    }

    async fn greet(&self, name: String, #[graphql(default = "Hello")] greeting: String) -> String {
        format!("{greeting}, {name}!")
    }

    async fn user(&self, id: i32) -> Option<User> {
        users().into_iter().find(|u| u.id == id)
    }

    async fn users(&self, order: Option<Order>) -> Vec<User> {
        let mut u = users();
        if order == Some(Order::Desc) {
            u.reverse();
        }
        u
    }

    async fn count_in(&self, range: Range) -> i32 {
        range.high - range.low
    }

    async fn character(&self, id: ID) -> Option<Character> {
        characters().into_iter().find(|c| match c {
            Character::Human(h) => h.id == id,
            Character::Droid(d) => d.id == id,
        })
    }

    async fn characters(&self) -> Vec<Character> {
        characters()
    }

    async fn search(&self) -> Vec<SearchResult> {
        vec![
            SearchResult::User(users().remove(0)),
            SearchResult::Droid(Droid {
                id: "2".into(),
                name: "R2-D2".into(),
                primary_function: "Astromech".into(),
            }),
        ]
    }

    /// Fails with a GraphQL error.
    async fn fail(&self) -> async_graphql::Result<Option<String>> {
        Err("intentional-error".into())
    }

    async fn partial(&self) -> Partial {
        Partial
    }

    async fn headers(&self, ctx: &Context<'_>) -> Vec<String> {
        ctx.data_unchecked::<SeenHeaders>().0.clone()
    }

    /// The body of the previous request this server received.
    async fn last_request(&self) -> Option<String> {
        LAST_REQUEST.lock().unwrap().clone()
    }
}

/// A value with a failing field: the response has both `data` and `errors`.
struct Partial;

#[Object]
impl Partial {
    async fn ok(&self) -> &str {
        "yes"
    }

    async fn bad(&self) -> async_graphql::Result<Option<String>> {
        Err("partial-error".into())
    }
}

struct Mutation;

#[Object]
impl Mutation {
    async fn create_user(&self, name: String) -> User {
        User { id: 3, name }
    }
}

type MockSchema = Schema<Query, Mutation, EmptySubscription>;

async fn mock_handler(
    axum::extract::State(schema): axum::extract::State<MockSchema>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    let previous = LAST_REQUEST.lock().unwrap().replace(body.clone());
    let req: GqlRequest = serde_json::from_str(&body).unwrap();
    // `lastRequest` must not overwrite what it reports.
    if req.query.contains("lastRequest") {
        *LAST_REQUEST.lock().unwrap() = previous;
    }
    let mut seen: Vec<String> = headers
        .iter()
        .filter(|(n, _)| n.as_str().starts_with("x-") || n.as_str() == "authorization")
        .map(|(n, v)| format!("{n}: {}", v.to_str().unwrap_or_default()))
        .collect();
    seen.sort();
    let partial = req.query.contains("partial");
    let resp = schema.execute(req.data(SeenHeaders(seen))).await;
    let mut body = serde_json::to_value(&resp).unwrap();
    // async-graphql nulls `data` on any error; real servers often send both.
    if partial {
        body["data"] = serde_json::json!({"partial": {"ok": "yes", "bad": null}});
    }
    let mut out = axum::response::IntoResponse::into_response(axum::Json(body));
    out.headers_mut()
        .append("set-cookie", "remote=1; Path=/".parse().unwrap());
    out
}

async fn start_mock() -> String {
    let schema = Schema::build(Query, Mutation, EmptySubscription).finish();
    let app = axum::Router::new()
        .route("/graphql", post(mock_handler))
        // Not a GraphQL server: introspection fails.
        .route("/broken", post(|| async { "not json" }))
        .with_state(schema);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

// --- case runner --------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    fixture: Option<String>,
    remote_schema_prioritize_data: bool,
    /// Expected inconsistencies: `name: reason`.
    inconsistencies: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    #[serde(default)]
    config: FileConfig,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    description: String,
    #[serde(default = "default_url")]
    url: String,
    #[serde(default = "default_status")]
    status: u16,
    #[serde(default)]
    headers: indexmap::IndexMap<String, String>,
    query: Value,
    response: Value,
    /// Expected `Set-Cookie` response headers.
    #[serde(default)]
    set_cookies: Option<Vec<String>>,
}

fn default_url() -> String {
    "/v1/graphql".to_owned()
}

fn default_status() -> u16 {
    200
}

async fn setup_database(url: &str, fixture: &Path) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect to test database");
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(fixture.join("schema.sql")).unwrap();
    client.batch_execute(&sql).await.expect("apply schema.sql");
}

fn case_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(CASES)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    files.sort();
    files
}

async fn run_file(path: &Path, database_url: &str) -> Vec<String> {
    let file: CaseFile = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let c = file.config;
    let fixture = Path::new(FIXTURES).join(c.fixture.as_deref().unwrap_or("remote_schemas"));
    setup_database(database_url, &fixture).await;
    let config = ServerConfig {
        database_url: Some(database_url.to_owned()),
        metadata_dir: Some(fixture.join("metadata")),
        remote_schema_prioritize_data: c.remote_schema_prioritize_data,
        env_overrides: test_env(),
        ..ServerConfig::default()
    };
    let engine = hge_server::build_engine(config)
        .await
        .expect("build engine");
    let mut failures = vec![];
    let actual: Vec<String> = engine
        .state
        .load()
        .inconsistencies
        .iter()
        .map(|o| format!("{}: {}", o.name, o.reason))
        .collect();
    if actual != c.inconsistencies {
        failures.push(format!(
            "{}: inconsistencies\n  expected: {:?}\n  actual:   {actual:?}",
            path.display(),
            c.inconsistencies
        ));
    }
    let router = hge_server::routes::router(engine);

    for case in file.cases {
        let mut req = Request::builder().method("POST").uri(&case.url);
        for (k, v) in &case.headers {
            req = req.header(k, v);
        }
        req = req.header("content-type", "application/json");
        let resp = router
            .clone()
            .oneshot(
                req.body(Body::from(serde_json::to_vec(&case.query).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let cookies: Vec<String> = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let actual: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        let cookies_ok = case.set_cookies.as_ref().is_none_or(|c| *c == cookies);
        if status != StatusCode::from_u16(case.status).unwrap()
            || actual != case.response
            || !cookies_ok
        {
            failures.push(format!(
                "{}: {}\n  expected ({}): {}\n  actual   ({}): {}\n  cookies: {cookies:?}",
                path.file_name().unwrap().to_string_lossy(),
                case.description,
                case.status,
                case.response,
                status.as_u16(),
                actual,
            ));
        }
    }
    failures
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_schema_cases() {
    let Ok(url) = std::env::var("HASURA_GRAPHQL_DATABASE_URL") else {
        eprintln!("HASURA_GRAPHQL_DATABASE_URL not set; skipping remote schema tests");
        return;
    };
    MOCK_URL.set(start_mock().await).expect("mock started once");
    let mut failures = vec![];
    for file in case_files() {
        failures.extend(run_file(&file, &url).await);
    }
    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
