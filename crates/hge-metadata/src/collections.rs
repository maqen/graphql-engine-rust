//! Query collections, the allowlist, RESTified endpoints and API limits (spec 13 §1-§4).
//!
//! Parsing follows V2's `FromJSON` instances (unknown keys are ignored, parse errors use
//! aeson's wording) and serialising follows its `ToJSON` (what `export_metadata` returns).

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// V2's `NonEmptyText`.
fn non_empty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.is_empty() {
        return Err(de::Error::custom("empty string not allowed"));
    }
    Ok(s)
}

/// A `NonEmpty` list.
fn non_empty_list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    let items = Vec::<T>::deserialize(d)?;
    if items.is_empty() {
        return Err(de::Error::custom(
            "parsing NonEmpty failed, unexpected empty list",
        ));
    }
    Ok(items)
}

/// A `query_collections` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryCollection {
    #[serde(deserialize_with = "non_empty")]
    pub name: String,
    pub definition: CollectionDefinition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionDefinition {
    pub queries: Vec<ListedQuery>,
}

/// A named GraphQL document in a collection. `query` is kept verbatim (V2's
/// `GQLQueryWithText`) but must parse as an executable document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListedQuery {
    pub name: String,
    pub query: String,
}

impl<'de> Deserialize<'de> for ListedQuery {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(deserialize_with = "non_empty")]
            name: String,
            query: String,
        }
        let raw = Raw::deserialize(d)?;
        if async_graphql_parser::parse_query(&raw.query).is_err() {
            return Err(de::Error::custom("parsing the graphql query failed"));
        }
        Ok(ListedQuery {
            name: raw.name,
            query: raw.query,
        })
    }
}

impl QueryCollection {
    pub fn query(&self, name: &str) -> Option<&ListedQuery> {
        self.definition.queries.iter().find(|q| q.name == name)
    }
}

/// An `allowlist` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllowlistEntry {
    pub collection: String,
    #[serde(default)]
    pub scope: AllowlistScope,
}

/// Which roles an allowlisted collection applies to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AllowlistScope {
    #[default]
    Global,
    /// A non-empty list of distinct roles.
    Roles(Vec<String>),
}

impl AllowlistScope {
    pub fn is_global(&self) -> bool {
        matches!(self, AllowlistScope::Global)
    }
}

impl Serialize for AllowlistScope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            AllowlistScope::Global => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("global", &true)?;
                m.end()
            }
            AllowlistScope::Roles(roles) => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("global", &false)?;
                m.serialize_entry("roles", roles)?;
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for AllowlistScope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            global: bool,
            #[serde(default)]
            roles: Option<Value>,
        }
        let raw = Raw::deserialize(d)?;
        match (raw.global, raw.roles) {
            (true, None) => Ok(AllowlistScope::Global),
            (true, Some(_)) => Err(de::Error::custom(
                "roles are not allowed when global is true",
            )),
            (false, None) => Err(de::Error::custom("key \"roles\" not found")),
            (false, Some(roles)) => {
                let roles: Vec<String> =
                    non_empty_list(roles).map_err(|e| de::Error::custom(e.to_string()))?;
                let mut seen = std::collections::HashSet::new();
                if !roles.iter().all(|r| seen.insert(r)) {
                    return Err(de::Error::custom("duplicate roles are not allowed"));
                }
                Ok(AllowlistScope::Roles(roles))
            }
        }
    }
}

/// A `rest_endpoints` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestEndpoint {
    #[serde(deserialize_with = "non_empty")]
    pub name: String,
    /// The path after `/api/rest/`; `:name` segments are variables.
    #[serde(deserialize_with = "non_empty")]
    pub url: String,
    #[serde(deserialize_with = "non_empty_list")]
    pub methods: Vec<EndpointMethod>,
    pub definition: EndpointDefinition,
    /// Exported as `null` when absent.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EndpointMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl EndpointMethod {
    pub const ALL: [EndpointMethod; 5] = [
        EndpointMethod::Get,
        EndpointMethod::Post,
        EndpointMethod::Put,
        EndpointMethod::Delete,
        EndpointMethod::Patch,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EndpointMethod::Get => "GET",
            EndpointMethod::Post => "POST",
            EndpointMethod::Put => "PUT",
            EndpointMethod::Delete => "DELETE",
            EndpointMethod::Patch => "PATCH",
        }
    }

    /// Case-sensitive, like V2.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

impl Serialize for EndpointMethod {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EndpointMethod {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        EndpointMethod::parse(&s).ok_or_else(|| {
            de::Error::custom(format!(
                "parsing Hasura.RQL.Types.Endpoint.EndpointMethod failed, expected one of the \
                 tags [\"GET\",\"POST\",\"PUT\",\"DELETE\",\"PATCH\"], but found tag \"{s}\""
            ))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointDefinition {
    pub query: QueryReference,
}

/// A query in a collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryReference {
    pub collection_name: String,
    pub query_name: String,
}

/// The `api_limits` object. CE stores it but enforces nothing (spec 13 §4).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ApiLimits {
    #[serde(default, deserialize_with = "null_as_default")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<Limit<RateLimit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_limit: Option<Limit<IntLimit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_limit: Option<Limit<IntLimit>>,
    /// Seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit: Option<Limit<Seconds>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_limit: Option<Limit<IntLimit>>,
}

impl ApiLimits {
    /// V2 omits `api_limits` from the export when it equals the empty value.
    pub fn is_empty(&self) -> bool {
        *self == ApiLimits::default()
    }
}

pub(crate) fn api_limits_absent(limits: &Option<ApiLimits>) -> bool {
    limits.as_ref().is_none_or(ApiLimits::is_empty)
}

fn null_as_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    d: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// `{"global": v, "per_role": {role: v}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Limit<T> {
    pub global: T,
    #[serde(default, deserialize_with = "null_as_default")]
    pub per_role: BTreeMap<String, T>,
}

impl<T> Limit<T> {
    /// The limit for `role`: its own, else the global one.
    pub fn for_role(&self, role: &str) -> &T {
        self.per_role.get(role).unwrap_or(&self.global)
    }
}

/// An `Int` as aeson parses it: fractional values are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct IntLimit(pub i64);

impl<'de> Deserialize<'de> for IntLimit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = serde_json::Number::deserialize(d)?;
        n.as_i64().map(IntLimit).ok_or_else(|| {
            de::Error::custom(format!(
                "parsing Int failed, value is either floating or will cause over or underflow {n}"
            ))
        })
    }
}

/// A `DiffTime` in seconds, kept as written.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Seconds(pub serde_json::Number);

impl<'de> Deserialize<'de> for Seconds {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) => Ok(Seconds(n)),
            other => Err(de::Error::custom(format!(
                "parsing DiffTime failed, expected Number, but encountered {}",
                aeson_type(&other)
            ))),
        }
    }
}

fn aeson_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_reqs_per_min: IntLimit,
    /// Exported as `null` when absent.
    #[serde(default)]
    pub unique_params: Option<UniqueParams>,
}

/// What a rate limit counts requests by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UniqueParams {
    Ip,
    /// Session variable names, as written.
    SessionVariables(Vec<String>),
}

const UNIQUE_PARAMS_ERROR: &str =
    "Not a valid value. Should be either: 'IP' or a list of Hasura session variables";

impl Serialize for UniqueParams {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            UniqueParams::Ip => s.serialize_str("IP"),
            UniqueParams::SessionVariables(vars) => vars.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for UniqueParams {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s.eq_ignore_ascii_case("ip") => Ok(UniqueParams::Ip),
            Value::Array(items) => items
                .into_iter()
                .map(|v| match v {
                    // V2 rejects exactly `x-hasura-role` and accepts other spellings of it.
                    Value::String(s)
                        if s.get(..9)
                            .is_some_and(|p| p.eq_ignore_ascii_case("x-hasura-"))
                            && s != "x-hasura-role" =>
                    {
                        Ok(s)
                    }
                    _ => Err(de::Error::custom(UNIQUE_PARAMS_ERROR)),
                })
                .collect::<Result<_, _>>()
                .map(UniqueParams::SessionVariables),
            _ => Err(de::Error::custom(UNIQUE_PARAMS_ERROR)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err<T: for<'de> Deserialize<'de> + std::fmt::Debug>(v: Value) -> String {
        serde_json::from_value::<T>(v).unwrap_err().to_string()
    }

    #[test]
    fn collection_round_trip_omits_missing_comment() {
        let v = json!({"name": "c", "definition": {"queries": [{"name": "q", "query": "query q { a }"}]}});
        let c: QueryCollection = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&c).unwrap(), v);
        assert_eq!(
            err::<QueryCollection>(
                json!({"name": "c", "definition": {"queries": [{"name": "q", "query": "query { a "}]}})
            ),
            "parsing the graphql query failed"
        );
        assert_eq!(
            err::<QueryCollection>(json!({"name": "", "definition": {"queries": []}})),
            "empty string not allowed"
        );
    }

    #[test]
    fn allowlist_scope() {
        let e: AllowlistEntry = serde_json::from_value(json!({"collection": "c"})).unwrap();
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"collection": "c", "scope": {"global": true}})
        );
        let s = |v| err::<AllowlistScope>(v);
        assert_eq!(
            s(json!({"global": true, "roles": ["a"]})),
            "roles are not allowed when global is true"
        );
        assert_eq!(s(json!({"global": false})), "key \"roles\" not found");
        assert_eq!(
            s(json!({"global": false, "roles": ["a", "a"]})),
            "duplicate roles are not allowed"
        );
        assert_eq!(
            s(json!({"global": false, "roles": []})),
            "parsing NonEmpty failed, unexpected empty list"
        );
        let roles = json!({"global": false, "roles": ["a", "b"]});
        let scope: AllowlistScope = serde_json::from_value(roles.clone()).unwrap();
        assert_eq!(serde_json::to_value(&scope).unwrap(), roles);
    }

    #[test]
    fn rest_endpoint() {
        let v = json!({"name": "e", "url": "a/:id", "methods": ["GET", "POST"],
            "definition": {"query": {"collection_name": "c", "query_name": "q"}}});
        let e: RestEndpoint = serde_json::from_value(v).unwrap();
        assert_eq!(serde_json::to_value(&e).unwrap()["comment"], Value::Null);
        assert_eq!(
            err::<RestEndpoint>(json!({"name": "e", "url": "x", "methods": ["get"],
                "definition": {"query": {"collection_name": "c", "query_name": "q"}}})),
            "parsing Hasura.RQL.Types.Endpoint.EndpointMethod failed, expected one of the tags \
             [\"GET\",\"POST\",\"PUT\",\"DELETE\",\"PATCH\"], but found tag \"get\""
        );
        assert_eq!(
            err::<RestEndpoint>(json!({"name": "e", "url": "", "methods": ["GET"],
                "definition": {"query": {"collection_name": "c", "query_name": "q"}}})),
            "empty string not allowed"
        );
    }

    #[test]
    fn api_limits_match_v2_export() {
        let l: ApiLimits = serde_json::from_value(json!({
            "rate_limit": {"global": {"max_reqs_per_min": 10, "unique_params": "ip"},
                "per_role": {"user": {"max_reqs_per_min": 3, "unique_params": ["X-Hasura-User-Id"]}}},
            "depth_limit": {"global": 5},
            "time_limit": {"global": 2.5, "per_role": {"u": 1}},
            "batch_limit": {"global": 2, "per_role": null},
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&l).unwrap(),
            json!({
                "disabled": false,
                "rate_limit": {"global": {"max_reqs_per_min": 10, "unique_params": "IP"},
                    "per_role": {"user": {"max_reqs_per_min": 3, "unique_params": ["X-Hasura-User-Id"]}}},
                "depth_limit": {"global": 5, "per_role": {}},
                "time_limit": {"global": 2.5, "per_role": {"u": 1}},
                "batch_limit": {"global": 2, "per_role": {}},
            })
        );
        assert!(
            serde_json::from_value::<ApiLimits>(json!({}))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            err::<ApiLimits>(json!({"depth_limit": {"global": 1.5}})),
            "parsing Int failed, value is either floating or will cause over or underflow 1.5"
        );
        assert_eq!(
            err::<ApiLimits>(json!({"time_limit": {"global": "3"}})),
            "parsing DiffTime failed, expected Number, but encountered String"
        );
        for bad in [
            json!("foo"),
            json!(["x-hasura-role"]),
            json!(["foo"]),
            json!(1),
        ] {
            assert_eq!(err::<UniqueParams>(bad), UNIQUE_PARAMS_ERROR);
        }
        for ok in [json!("Ip"), json!(["X-Hasura-Role"]), json!([])] {
            serde_json::from_value::<UniqueParams>(ok).unwrap();
        }
    }
}
