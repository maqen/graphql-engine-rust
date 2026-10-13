//! Environment lookups for URL and header templates.

use std::sync::Arc;

/// Reads an environment variable. Injected so tests and embedders can supply values.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Substitute `{{VAR}}` occurrences (surrounding spaces inside the braces allowed). On
/// failure returns the names of the missing variables.
pub fn resolve_template(
    template: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, Vec<String>> {
    let mut out = String::with_capacity(template.len());
    let mut missing = vec![];
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        out.push_str(&rest[..start]);
        let var = rest[start + 2..start + 2 + end].trim();
        match env(var) {
            Some(v) => out.push_str(&v),
            None => missing.push(var.to_owned()),
        }
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        (name == "HOST").then(|| "http://localhost:4000".to_owned())
    }

    #[test]
    fn templates() {
        assert_eq!(
            resolve_template("{{HOST}}/graphql", &env).unwrap(),
            "http://localhost:4000/graphql"
        );
        assert_eq!(
            resolve_template("{{ HOST }}/x", &env).unwrap(),
            "http://localhost:4000/x"
        );
        assert_eq!(
            resolve_template("{{NOPE}}/x", &env).unwrap_err(),
            vec!["NOPE".to_owned()]
        );
        assert_eq!(resolve_template("plain", &env).unwrap(), "plain");
    }
}
