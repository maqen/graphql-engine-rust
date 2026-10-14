//! Prometheus metrics at `GET /v1/metrics`, V2 EE's names, types, buckets and labels
//! (`Hasura.Server.Prometheus`).
//!
//! On only with `metrics` in `HASURA_GRAPHQL_ENABLED_APIS`; until [`enable`] is called every
//! recording function returns after one relaxed load. Values are process-wide atomics, so
//! recording never takes a lock. `HASURA_GRAPHQL_METRICS_SECRET`, when set, must be sent as
//! `Authorization: Bearer <secret>`.
//!
//! Not yet recorded: V2's high-cardinality labels (`operation_name`,
//! `parameterized_query_hash`), and the event trigger, scheduled trigger, subscription poller
//! timing and Postgres pool metrics.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn recording on (at startup, when the metrics API is enabled).
pub fn enable() {
    ENABLED.store(true, Relaxed);
}

/// Whether metrics are recorded.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// V2's `operation_type` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Query,
    Mutation,
    Subscription,
    /// The request failed before its operation type was known (a parse error).
    Unknown,
}

impl OperationType {
    const ALL: [OperationType; 4] = [
        OperationType::Query,
        OperationType::Mutation,
        OperationType::Subscription,
        OperationType::Unknown,
    ];

    fn label(self) -> &'static str {
        match self {
            OperationType::Query => "query",
            OperationType::Mutation => "mutation",
            OperationType::Subscription => "subscription",
            OperationType::Unknown => "unknown",
        }
    }
}

impl From<hge_ir::OperationKind> for OperationType {
    fn from(kind: hge_ir::OperationKind) -> Self {
        match kind {
            hge_ir::OperationKind::Query => OperationType::Query,
            hge_ir::OperationKind::Mutation => OperationType::Mutation,
            hge_ir::OperationKind::Subscription => OperationType::Subscription,
        }
    }
}

/// V2's `subscription_kind` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionKind {
    LiveQuery,
    Streaming,
}

impl SubscriptionKind {
    fn label(self) -> &'static str {
        match self {
            SubscriptionKind::LiveQuery => "live-query",
            SubscriptionKind::Streaming => "streaming",
        }
    }
}

const EXECUTION_BUCKETS: [f64; 7] = [0.01, 0.03, 0.1, 0.3, 1.0, 3.0, 10.0];

/// A histogram with fixed buckets; the sum is kept in nanoseconds. The count is the sum of
/// the buckets as read, so a scrape racing an observation stays monotonic.
struct Histogram<const N: usize> {
    buckets: [AtomicU64; N],
    /// Observations above the last bound.
    overflow: AtomicU64,
    sum_nanos: AtomicU64,
}

impl<const N: usize> Histogram<N> {
    const fn new() -> Self {
        Histogram {
            buckets: [const { AtomicU64::new(0) }; N],
            overflow: AtomicU64::new(0),
            sum_nanos: AtomicU64::new(0),
        }
    }

    fn observe(&self, bounds: &[f64; N], d: Duration) {
        let secs = d.as_secs_f64();
        // Buckets are stored non-cumulatively and summed when rendered.
        match bounds.iter().position(|b| secs <= *b) {
            Some(i) => self.buckets[i].fetch_add(1, Relaxed),
            None => self.overflow.fetch_add(1, Relaxed),
        };
        self.sum_nanos
            .fetch_add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX), Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, labels: &str, bounds: &[f64; N]) {
        let mut cumulative = 0;
        for (bound, bucket) in bounds.iter().zip(&self.buckets) {
            cumulative += bucket.load(Relaxed);
            let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
        }
        let count = cumulative + self.overflow.load(Relaxed);
        let sum = self.sum_nanos.load(Relaxed) as f64 / 1e9;
        let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_sum{{{labels}}} {sum}");
        let _ = writeln!(out, "{name}_count{{{labels}}} {count}");
    }
}

struct Metrics {
    /// By operation type, then success (0) or failure (1).
    graphql_requests: [[AtomicU64; 2]; 4],
    /// Queries (0) and mutations (1).
    graphql_execution_time: [Histogram<7>; 2],
    http_connections: AtomicI64,
    websocket_connections: AtomicI64,
    /// Live queries (0) and streams (1).
    active_subscriptions: [AtomicI64; 2],
    websocket_received_bytes: AtomicU64,
    websocket_sent_bytes: AtomicU64,
    http_request_bytes: AtomicU64,
    http_response_bytes: AtomicU64,
}

static METRICS: Metrics = Metrics {
    graphql_requests: [const { [const { AtomicU64::new(0) }; 2] }; 4],
    graphql_execution_time: [const { Histogram::new() }; 2],
    http_connections: AtomicI64::new(0),
    websocket_connections: AtomicI64::new(0),
    active_subscriptions: [const { AtomicI64::new(0) }; 2],
    websocket_received_bytes: AtomicU64::new(0),
    websocket_sent_bytes: AtomicU64::new(0),
    http_request_bytes: AtomicU64::new(0),
    http_response_bytes: AtomicU64::new(0),
};

/// A GraphQL operation finished (over HTTP or WebSocket): `hasura_graphql_requests_total`,
/// and for queries and mutations `hasura_graphql_execution_time_seconds`. V2's docs say the
/// histogram covers successful requests, but its source (`GraphQL/Transport/HTTP.hs`)
/// observes failed ones too; this follows the source.
pub fn graphql_request(op: OperationType, success: bool, elapsed: Duration) {
    if !enabled() {
        return;
    }
    METRICS.graphql_requests[op as usize][usize::from(!success)].fetch_add(1, Relaxed);
    let histogram = match op {
        OperationType::Query => &METRICS.graphql_execution_time[0],
        OperationType::Mutation => &METRICS.graphql_execution_time[1],
        _ => return,
    };
    histogram.observe(&EXECUTION_BUCKETS, elapsed);
}

/// Bytes read from a WebSocket client.
pub fn websocket_received(bytes: usize) {
    if enabled() {
        METRICS
            .websocket_received_bytes
            .fetch_add(bytes as u64, Relaxed);
    }
}

/// Bytes sent to a WebSocket client.
pub fn websocket_sent(bytes: usize) {
    if enabled() {
        METRICS
            .websocket_sent_bytes
            .fetch_add(bytes as u64, Relaxed);
    }
}

/// The size of an HTTP request body and its response body.
pub fn http_bytes(request: u64, response: u64) {
    if enabled() {
        METRICS.http_request_bytes.fetch_add(request, Relaxed);
        METRICS.http_response_bytes.fetch_add(response, Relaxed);
    }
}

/// Counts one in a gauge while alive.
pub struct GaugeGuard(Option<&'static AtomicI64>);

impl GaugeGuard {
    fn new(gauge: &'static AtomicI64) -> GaugeGuard {
        if !enabled() {
            return GaugeGuard(None);
        }
        gauge.fetch_add(1, Relaxed);
        GaugeGuard(Some(gauge))
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        if let Some(gauge) = self.0 {
            gauge.fetch_sub(1, Relaxed);
        }
    }
}

/// An HTTP request in flight (`hasura_http_connections`).
pub fn http_connection() -> GaugeGuard {
    GaugeGuard::new(&METRICS.http_connections)
}

/// An open WebSocket connection (`hasura_websocket_connections`).
pub fn websocket_connection() -> GaugeGuard {
    GaugeGuard::new(&METRICS.websocket_connections)
}

/// A running subscription (`hasura_active_subscriptions`).
pub fn active_subscription(kind: SubscriptionKind) -> GaugeGuard {
    GaugeGuard::new(&METRICS.active_subscriptions[kind as usize])
}

/// Route middleware (on matched routes only, so unknown paths are not counted):
/// `hasura_http_connections` and the HTTP byte counters. Like V2, `/healthz`,
/// `/v1/version` and WebSocket connections are left out.
pub async fn http_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header;
    use http_body::Body as _;

    let path = req.uri().path();
    let upgrade = req.headers().contains_key(header::UPGRADE);
    if !enabled() || upgrade || matches!(path, "/healthz" | "/hasura/healthz" | "/v1/version") {
        return next.run(req).await;
    }
    // Released when the handler returns: a streamed body (pg_dump) is not counted while it
    // is sent, and contributes no response bytes.
    let _connection = http_connection();
    let request = req.body().size_hint().exact().unwrap_or_else(|| {
        req.headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse().ok())
            .unwrap_or(0)
    });
    let resp = next.run(req).await;
    http_bytes(request, resp.body().size_hint().exact().unwrap_or(0));
    resp
}

/// The Prometheus text exposition (format 0.0.4).
pub fn render() -> String {
    let m = &METRICS;
    let mut out = String::with_capacity(4096);
    header(
        &mut out,
        "hasura_graphql_requests_total",
        "counter",
        "Number of GraphQL requests received",
    );
    for op in OperationType::ALL {
        for (i, status) in ["success", "failed"].into_iter().enumerate() {
            let n = m.graphql_requests[op as usize][i].load(Relaxed);
            let _ = writeln!(
                out,
                "hasura_graphql_requests_total{{operation_type=\"{}\",response_status=\"{status}\"}} {n}",
                op.label()
            );
        }
    }
    header(
        &mut out,
        "hasura_graphql_execution_time_seconds",
        "histogram",
        "Execution time of successful GraphQL requests (excluding subscriptions)",
    );
    for (i, op) in ["query", "mutation"].into_iter().enumerate() {
        m.graphql_execution_time[i].render(
            &mut out,
            "hasura_graphql_execution_time_seconds",
            &format!("operation_type=\"{op}\""),
            &EXECUTION_BUCKETS,
        );
    }
    gauge(
        &mut out,
        "hasura_http_connections",
        "Current number of active HTTP connections (excluding WebSocket connections)",
        m.http_connections.load(Relaxed),
    );
    gauge(
        &mut out,
        "hasura_websocket_connections",
        "Current number of active WebSocket connections",
        m.websocket_connections.load(Relaxed),
    );
    header(
        &mut out,
        "hasura_active_subscriptions",
        "gauge",
        "Current number of active subscriptions",
    );
    for kind in [SubscriptionKind::LiveQuery, SubscriptionKind::Streaming] {
        let n = m.active_subscriptions[kind as usize].load(Relaxed);
        let _ = writeln!(
            out,
            "hasura_active_subscriptions{{subscription_kind=\"{}\"}} {n}",
            kind.label()
        );
    }
    counter(
        &mut out,
        "hasura_websocket_messages_received_bytes_total",
        "Total number of bytes received over WebSocket connections",
        m.websocket_received_bytes.load(Relaxed),
    );
    counter(
        &mut out,
        "hasura_websocket_messages_sent_bytes_total",
        "Total number of bytes sent over WebSocket connections",
        m.websocket_sent_bytes.load(Relaxed),
    );
    counter(
        &mut out,
        "hasura_http_request_bytes_total",
        "Total size of HTTP request bodies",
        m.http_request_bytes.load(Relaxed),
    );
    counter(
        &mut out,
        "hasura_http_response_bytes_total",
        "Total size of HTTP response bodies",
        m.http_response_bytes.load(Relaxed),
    );
    out
}

fn header(out: &mut String, name: &str, ty: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {ty}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: i64) {
    header(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    header(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::<7>::new();
        h.observe(&EXECUTION_BUCKETS, Duration::from_millis(5));
        h.observe(&EXECUTION_BUCKETS, Duration::from_millis(50));
        h.observe(&EXECUTION_BUCKETS, Duration::from_secs(20));
        let mut out = String::new();
        h.render(&mut out, "h", "a=\"b\"", &EXECUTION_BUCKETS);
        assert!(out.contains("h_bucket{a=\"b\",le=\"0.01\"} 1\n"), "{out}");
        assert!(out.contains("h_bucket{a=\"b\",le=\"0.1\"} 2\n"), "{out}");
        assert!(out.contains("h_bucket{a=\"b\",le=\"10\"} 2\n"), "{out}");
        assert!(out.contains("h_bucket{a=\"b\",le=\"+Inf\"} 3\n"), "{out}");
        assert!(out.contains("h_sum{a=\"b\"} 20.055\n"), "{out}");
        assert!(out.contains("h_count{a=\"b\"} 3\n"), "{out}");
    }
}
