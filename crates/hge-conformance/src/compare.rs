//! Response comparison.
//!
//! Expected responses are recorded from Hasura itself, so comparison is exact, with one
//! relaxation taken from tests-py: object key order only matters inside GraphQL `data`
//! (where it follows the selection set), not in errors or non-GraphQL responses.

use serde_json::Value;

/// Endpoints whose responses are GraphQL responses.
pub fn is_graphql_url(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    matches!(
        path,
        "/v1/graphql" | "/v1alpha1/graphql" | "/v1/relay" | "/v1beta1/relay"
    )
}

/// `None` when equal, otherwise the JSON path of the first difference and why.
pub fn diff(expected: &Value, actual: &Value, graphql: bool) -> Option<String> {
    if graphql {
        // A batched request returns an array of GraphQL responses.
        if let (Value::Array(e), Value::Array(a)) = (expected, actual)
            && e.len() == a.len()
        {
            return e
                .iter()
                .zip(a)
                .enumerate()
                .find_map(|(i, (e, a))| response_diff(e, a, &format!("$[{i}]")));
        }
        return response_diff(expected, actual, "$");
    }
    value_diff(expected, actual, "$", false)
}

fn response_diff(expected: &Value, actual: &Value, path: &str) -> Option<String> {
    let (Value::Object(e), Value::Object(a)) = (expected, actual) else {
        return value_diff(expected, actual, path, false);
    };
    let mut ek: Vec<_> = e.keys().collect();
    let mut ak: Vec<_> = a.keys().collect();
    ek.sort();
    ak.sort();
    if ek != ak {
        return Some(format!("{path}: keys {ek:?} != {ak:?}"));
    }
    e.iter().find_map(|(k, ev)| {
        let ordered = k == "data";
        value_diff(ev, &a[k], &format!("{path}.{k}"), ordered)
    })
}

fn value_diff(expected: &Value, actual: &Value, path: &str, ordered: bool) -> Option<String> {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            if ordered {
                let ek: Vec<_> = e.keys().collect();
                let ak: Vec<_> = a.keys().collect();
                if ek != ak {
                    return Some(format!("{path}: keys {ek:?} != {ak:?}"));
                }
            } else {
                let mut ek: Vec<_> = e.keys().collect();
                let mut ak: Vec<_> = a.keys().collect();
                ek.sort();
                ak.sort();
                if ek != ak {
                    return Some(format!("{path}: keys {ek:?} != {ak:?}"));
                }
            }
            e.iter()
                .find_map(|(k, ev)| value_diff(ev, &a[k], &format!("{path}.{k}"), ordered))
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                return Some(format!("{path}: {} items != {} items", e.len(), a.len()));
            }
            e.iter()
                .zip(a)
                .enumerate()
                .find_map(|(i, (e, a))| value_diff(e, a, &format!("{path}[{i}]"), ordered))
        }
        (Value::Number(e), Value::Number(a)) if e == a || floats_close(e, a) => None,
        (e, a) if e == a => None,
        (e, a) => Some(format!("{path}: {} != {}", short(e), short(a))),
    }
}

/// Postgres float aggregates (`stddev`, `avg` over floats) can differ in the last bit
/// between runs, even on V2 itself, depending on the order rows are accumulated in.
/// Integers compare exactly; an integer equals a float of the same exactly representable value.
fn floats_close(e: &serde_json::Number, a: &serde_json::Number) -> bool {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    let (Some(x), Some(y)) = (e.as_f64(), a.as_f64()) else {
        return false;
    };
    match (e.is_f64(), a.is_f64()) {
        (true, true) => x == y || (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
        (true, false) | (false, true) => x == y && x.abs() < EXACT,
        (false, false) => false,
    }
}

fn short(v: &Value) -> String {
    clip(&v.to_string(), 200)
}

/// `s` cut to at most `max` characters.
pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn data_key_order_matters() {
        let e = json!({"data": {"a": 1, "b": 2}});
        let a = json!({"data": {"b": 2, "a": 1}});
        assert!(diff(&e, &a, true).is_some());
        assert!(diff(&e, &e, true).is_none());
    }

    #[test]
    fn error_key_order_does_not() {
        let e = json!({"errors": [{"message": "m", "extensions": {"path": "$", "code": "c"}}]});
        let a = json!({"errors": [{"extensions": {"code": "c", "path": "$"}, "message": "m"}]});
        assert!(diff(&e, &a, true).is_none());
        assert!(diff(&e, &a, false).is_none());
    }

    #[test]
    fn reports_the_differing_path() {
        let e = json!({"data": {"t": [{"id": 1}, {"id": 2}]}});
        let a = json!({"data": {"t": [{"id": 1}, {"id": 3}]}});
        assert_eq!(diff(&e, &a, true).unwrap(), "$.data.t[1].id: 2 != 3");
        let a = json!({"data": {"t": [{"id": 1}]}});
        assert_eq!(diff(&e, &a, true).unwrap(), "$.data.t: 2 items != 1 items");
    }

    #[test]
    fn batched_responses() {
        let e = json!([{"data": {"a": 1}}, {"data": {"b": 2}}]);
        let a = json!([{"data": {"a": 1}}, {"data": {"b": 3}}]);
        assert_eq!(diff(&e, &a, true).unwrap(), "$[1].data.b: 2 != 3");
    }

    #[test]
    fn numbers_compare_by_value() {
        assert!(diff(&json!(1.5), &json!(1.5), false).is_none());
        assert!(diff(&json!(1), &json!(1.0), false).is_none());
        assert!(diff(&json!(1), &json!(2), false).is_some());
        assert!(
            diff(
                &json!(1_234_567_890_123_456_u64),
                &json!(1_234_567_890_122_999_u64),
                false
            )
            .is_some()
        );
        assert!(diff(&json!(u64::MAX), &json!(u64::MAX - 1), false).is_some());
        assert!(
            diff(
                &json!(0.123_456_789_012_345_6),
                &json!(0.123_456_789_012_345_5),
                false
            )
            .is_none()
        );
        assert!(
            diff(
                &json!(0.123_456_789_012_345_6),
                &json!(0.123_456_789_013),
                false
            )
            .is_some()
        );
    }

    #[test]
    fn graphql_urls() {
        assert!(is_graphql_url("/v1/graphql"));
        assert!(is_graphql_url("/v1beta1/relay"));
        assert!(!is_graphql_url("/v1/query"));
    }
}
