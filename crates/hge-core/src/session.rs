//! Roles and session variables.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

pub const ADMIN_ROLE: &str = "admin";

/// A Hasura role name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoleName(pub Arc<str>);

impl RoleName {
    pub fn new(name: impl AsRef<str>) -> Self {
        RoleName(Arc::from(name.as_ref()))
    }

    pub fn admin() -> Self {
        Self::new(ADMIN_ROLE)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_admin(&self) -> bool {
        &*self.0 == ADMIN_ROLE
    }
}

impl fmt::Display for RoleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Session variables (`x-hasura-*`), keyed by lower-cased name as V2 does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionVariables(IndexMap<String, String>);

impl SessionVariables {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a variable; the name is lower-cased.
    pub fn insert(&mut self, name: &str, value: impl Into<String>) {
        self.0.insert(name.to_ascii_lowercase(), value.into());
    }

    /// Case-insensitive lookup.
    pub fn get(&self, name: &str) -> Option<&str> {
        if name.bytes().any(|b| b.is_ascii_uppercase()) {
            self.0.get(&name.to_ascii_lowercase()).map(String::as_str)
        } else {
            self.0.get(name).map(String::as_str)
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        )
    }

    /// The variables as V2 records them (`hasura.user`, an event's `session_variables`): a
    /// JSON object with sorted keys.
    pub fn to_sorted_json_string(&self) -> String {
        let sorted: std::collections::BTreeMap<&str, &str> = self.iter().collect();
        serde_json::to_string(&sorted).unwrap_or_default()
    }
}

/// `true` if a header / claim name is a session variable (`x-hasura-` prefix).
pub fn is_session_variable(name: &str) -> bool {
    name.as_bytes()
        .get(..9)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"x-hasura-"))
}

/// A resolved request session: who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub role: RoleName,
    pub variables: SessionVariables,
    /// Backend-only permissions may be used (`x-hasura-use-backend-only-permissions`; only
    /// honoured with a valid admin secret or when no auth is configured).
    pub backend_only: bool,
}

impl Session {
    pub fn admin() -> Self {
        let mut variables = SessionVariables::new();
        variables.insert("x-hasura-role", ADMIN_ROLE);
        Session {
            role: RoleName::admin(),
            variables,
            backend_only: false,
        }
    }

    pub fn is_admin(&self) -> bool {
        self.role.is_admin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_json_matches_v2() {
        let mut vars = SessionVariables::new();
        vars.insert("X-Hasura-ZZ", "1");
        vars.insert("x-hasura-role", "admin");
        vars.insert("x-hasura-aa", "2");
        assert_eq!(
            vars.to_sorted_json_string(),
            r#"{"x-hasura-aa":"2","x-hasura-role":"admin","x-hasura-zz":"1"}"#
        );
    }

    #[test]
    fn session_vars_are_case_insensitive() {
        let mut vars = SessionVariables::new();
        vars.insert("X-Hasura-User-Id", "42");
        assert_eq!(vars.get("x-hasura-user-id"), Some("42"));
        assert_eq!(vars.get("X-HASURA-USER-ID"), Some("42"));
        assert!(is_session_variable("X-Hasura-Role"));
        assert!(!is_session_variable("authorization"));
        assert!(!is_session_variable("12345678é"));
        assert!(!is_session_variable("x-hasuraé-role"));
    }
}
