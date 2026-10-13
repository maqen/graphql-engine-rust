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
}

/// `true` if a header / claim name is a session variable (`x-hasura-` prefix).
pub fn is_session_variable(name: &str) -> bool {
    name.len() >= 9 && name[..9].eq_ignore_ascii_case("x-hasura-")
}

/// A resolved request session: who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub role: RoleName,
    pub variables: SessionVariables,
}

impl Session {
    pub fn admin() -> Self {
        let mut variables = SessionVariables::new();
        variables.insert("x-hasura-role", ADMIN_ROLE);
        Session {
            role: RoleName::admin(),
            variables,
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
    fn session_vars_are_case_insensitive() {
        let mut vars = SessionVariables::new();
        vars.insert("X-Hasura-User-Id", "42");
        assert_eq!(vars.get("x-hasura-user-id"), Some("42"));
        assert_eq!(vars.get("X-HASURA-USER-ID"), Some("42"));
        assert!(is_session_variable("X-Hasura-Role"));
        assert!(!is_session_variable("authorization"));
    }
}
