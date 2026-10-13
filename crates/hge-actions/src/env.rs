//! Environment lookups and `{{VAR}}` templates (V2's `Data.URL.Template`).

use std::collections::HashMap;

/// Where `{{VAR}}` and `value_from_env` values come from.
pub trait Environment: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

/// The process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnv;

impl Environment for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Fixed values in front of another environment (tests, embedding).
pub struct Overrides<E> {
    pub values: HashMap<String, String>,
    pub fallback: E,
}

impl<E: Environment> Environment for Overrides<E> {
    fn get(&self, name: &str) -> Option<String> {
        self.values
            .get(name)
            .cloned()
            .or_else(|| self.fallback.get(name))
    }
}

/// A text with `{{VAR}}` segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Var(String),
}

impl Template {
    /// Parse a template. The variable name is everything between `{{` and the next `}}`
    /// (not trimmed). An unclosed `{{` is an error (V2 loops on it).
    pub fn parse(s: &str) -> Result<Template, String> {
        let mut parts = vec![];
        let mut rest = s;
        while let Some(start) = rest.find("{{") {
            if start > 0 {
                parts.push(Part::Text(rest[..start].to_owned()));
            }
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| format!("unclosed '{{{{' in template: {s}"))?;
            parts.push(Part::Var(after[..end].to_owned()));
            rest = &after[end + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_owned()));
        }
        Ok(Template { parts })
    }

    /// Substitute variables; `Err` lists the missing ones in order.
    pub fn resolve(&self, env: &dyn Environment) -> Result<String, Vec<String>> {
        let mut out = String::new();
        let mut missing = vec![];
        for part in &self.parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Var(v) => match env.get(v) {
                    Some(value) => out.push_str(&value),
                    None => missing.push(v.clone()),
                },
            }
        }
        if missing.is_empty() {
            Ok(out)
        } else {
            Err(missing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env;
    impl Environment for Env {
        fn get(&self, name: &str) -> Option<String> {
            (name == "BASE").then(|| "http://h:1".to_owned())
        }
    }

    #[test]
    fn templates() {
        let t = Template::parse("{{BASE}}/x/{{ NOPE }}").unwrap();
        assert_eq!(t.resolve(&Env), Err(vec![" NOPE ".to_owned()]));
        let t = Template::parse("{{BASE}}/login").unwrap();
        assert_eq!(t.resolve(&Env).unwrap(), "http://h:1/login");
        assert!(Template::parse("http://{{oops").is_err());
        assert_eq!(
            Template::parse("plain").unwrap().resolve(&Env).unwrap(),
            "plain"
        );
    }
}
