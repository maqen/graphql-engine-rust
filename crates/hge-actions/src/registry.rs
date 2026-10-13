//! Per-metadata-version handler configuration of every action.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use hge_core::InconsistentObject;
use hge_ir::action::ActionOutput;
use hge_metadata::Metadata;
use hge_metadata::actions::{Action, ActionType, HeaderConf};
use hge_transforms::{Request as TransformRequest, RequestTransform, ResponseTransform};

use crate::env::{Environment, Template};

/// Engine-wide settings for calling handlers.
#[derive(Debug, Clone)]
pub struct HandlerConfig {
    /// `User-Agent` sent to handlers (`hasura-graphql-engine/<version>`).
    pub user_agent: String,
    /// `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE`: configured headers win over forwarded
    /// client headers.
    pub configured_header_precedence: bool,
    /// Shared HTTP client (connection pool).
    pub client: reqwest::Client,
}

impl Default for HandlerConfig {
    fn default() -> Self {
        HandlerConfig {
            user_agent: "hasura-graphql-engine/v2.51.0".to_owned(),
            configured_header_precedence: false,
            client: reqwest::Client::new(),
        }
    }
}

/// How to call one action's handler.
#[derive(Debug, Clone)]
pub struct ActionHandler {
    pub name: String,
    pub action_type: ActionType,
    /// The handler as written (shown in error details instead of the resolved URL, so
    /// secrets in environment variables do not leak).
    pub handler_template: String,
    /// The resolved handler URL.
    pub url: String,
    pub headers: Vec<ConfiguredHeader>,
    pub forward_client_headers: bool,
    /// Lower-cased names of client headers that are never forwarded.
    pub ignored_client_headers: HashSet<String>,
    pub timeout: Duration,
    pub request_transform: Option<RequestTransform>,
    pub response_transform: Option<ResponseTransform>,
    /// With a request transform: the resolved URL parsed as V2's `parseRequest` does (the
    /// error is the invalid-URL message).
    pub transform_base: Option<Result<TransformRequest, String>>,
    pub output: Arc<ActionOutput>,
}

/// A configured header, with its value template parsed (values are resolved per request).
#[derive(Debug, Clone)]
pub enum ConfiguredHeader {
    Value { name: String, value: Template },
    FromEnv { name: String, env: String },
}

impl ConfiguredHeader {
    pub fn name(&self) -> &str {
        match self {
            ConfiguredHeader::Value { name, .. } | ConfiguredHeader::FromEnv { name, .. } => name,
        }
    }
}

/// All actions of one metadata version, by name.
pub struct ActionRegistry {
    pub actions: HashMap<String, Arc<ActionHandler>>,
    pub env: Arc<dyn Environment>,
    pub config: HandlerConfig,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        ActionRegistry {
            actions: HashMap::new(),
            env: Arc::new(crate::ProcessEnv),
            config: HandlerConfig::default(),
        }
    }
}

impl std::fmt::Debug for ActionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionRegistry")
            .field("actions", &self.actions.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The result of [`ActionRegistry::build`].
pub struct BuiltRegistry {
    pub registry: ActionRegistry,
    /// Actions that could not be registered (and must be dropped from the schema).
    pub failed: Vec<String>,
    pub inconsistencies: Vec<InconsistentObject>,
}

impl ActionRegistry {
    pub fn empty(env: Arc<dyn Environment>, config: HandlerConfig) -> Self {
        ActionRegistry {
            actions: HashMap::new(),
            env,
            config,
        }
    }

    /// Register every action whose output resolved (`outputs`), resolving handler URLs from
    /// the environment.
    pub fn build(
        metadata: &Metadata,
        outputs: &HashMap<String, Arc<ActionOutput>>,
        env: Arc<dyn Environment>,
        config: HandlerConfig,
    ) -> BuiltRegistry {
        let mut actions = HashMap::new();
        let mut failed = vec![];
        let mut inconsistencies = vec![];
        for raw in &metadata.actions {
            let Ok(action) = Action::from_json(raw) else {
                continue; // reported by the schema resolution
            };
            let Some(output) = outputs.get(&action.name) else {
                continue;
            };
            let def = action.definition;
            let url = Template::parse(&def.handler)
                .map_err(|e| format!("Parsing URL template failed: {e}"))
                .and_then(|t| {
                    t.resolve(env.as_ref()).map_err(|missing| {
                        format!(
                            "Value for environment variables not found: {}",
                            missing.join(", ")
                        )
                    })
                });
            let headers = def
                .headers
                .iter()
                .map(|h| match h {
                    HeaderConf::Value { name, value } => Template::parse(value)
                        .map(|value| ConfiguredHeader::Value {
                            name: name.clone(),
                            value,
                        })
                        .map_err(|e| format!("Parsing header template failed: {e}")),
                    HeaderConf::FromEnv { name, env } => Ok(ConfiguredHeader::FromEnv {
                        name: name.clone(),
                        env: env.clone(),
                    }),
                })
                .collect::<Result<Vec<_>, _>>();
            let (url, headers) = match url.and_then(|u| headers.map(|h| (u, h))) {
                Ok(v) => v,
                Err(e) => {
                    inconsistencies.push(InconsistentObject::action(
                        &action.name,
                        format!("in action \"{}\"; {e}", action.name),
                        raw.clone(),
                    ));
                    failed.push(action.name);
                    continue;
                }
            };
            // The handler URL as transforms see it, parsed once (only if one is configured).
            let transform_base = def
                .request_transform
                .as_ref()
                .map(|_| TransformRequest::new("POST", &url, vec![], vec![]));
            actions.insert(
                action.name.clone(),
                Arc::new(ActionHandler {
                    name: action.name,
                    action_type: def.action_type,
                    handler_template: def.handler,
                    url,
                    headers,
                    forward_client_headers: def.forward_client_headers,
                    ignored_client_headers: def
                        .ignored_client_headers
                        .iter()
                        .map(|h| h.to_ascii_lowercase())
                        .collect(),
                    timeout: Duration::from_secs(def.timeout_secs),
                    request_transform: def.request_transform,
                    response_transform: def.response_transform,
                    transform_base,
                    output: output.clone(),
                }),
            );
        }
        BuiltRegistry {
            registry: ActionRegistry {
                actions,
                env,
                config,
            },
            failed,
            inconsistencies,
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<ActionHandler>> {
        self.actions.get(name)
    }

    pub fn has_async_actions(&self) -> bool {
        self.actions.values().any(|a| {
            a.action_type == ActionType::Mutation(hge_metadata::actions::ActionKind::Asynchronous)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hge_ir::action::{OutputBase, OutputType};
    use serde_json::json;

    #[test]
    fn unclosed_header_template_is_inconsistent() {
        let metadata = hge_metadata::from_json(json!({
            "version": 3, "sources": [],
            "actions": [
                {"name": "bad", "definition": {"output_type": "String", "handler": "http://h",
                    "headers": [{"name": "X-A", "value": "{{OOPS"}]}},
                {"name": "good", "definition": {"output_type": "String", "handler": "http://h",
                    "headers": [{"name": "X-A", "value": "{{LATER}}"}]}}
            ]
        }))
        .unwrap();
        let output = Arc::new(ActionOutput {
            ty: OutputType::Named("String".into()),
            base: OutputBase::BuiltinScalar,
        });
        let outputs = [("bad", &output), ("good", &output)]
            .into_iter()
            .map(|(n, o)| (n.to_owned(), o.clone()))
            .collect();
        let built = ActionRegistry::build(
            &metadata,
            &outputs,
            Arc::new(crate::ProcessEnv),
            HandlerConfig::default(),
        );
        assert_eq!(built.failed, vec!["bad"]);
        let reasons: Vec<&str> = built
            .inconsistencies
            .iter()
            .map(|o| o.reason.as_str())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "in action \"bad\"; Parsing header template failed: unclosed '{{' in template: {{OOPS"
            ]
        );
        assert!(built.registry.get("good").is_some());
    }

    #[test]
    fn transforms_are_parsed_with_the_metadata() {
        // Malformed transform JSON makes the metadata unreadable, as in V2's FromJSON.
        let e = hge_metadata::from_json(json!({
            "version": 3, "sources": [],
            "actions": [
                {"name": "ok", "definition": {"output_type": "String", "handler": "http://h"}},
                {"name": "bad", "definition": {"output_type": "String", "handler": "http://h",
                    "request_transform": {"version": 2, "body": {"action": "nope"}}}}
            ]
        }))
        .unwrap_err();
        assert_eq!(
            (e.code, e.path.as_str(), e.message.as_str()),
            (
                hge_core::Code::ParseFailed,
                "$.actions[1].definition.request_transform.body",
                "invalid transform action"
            )
        );
        let e = hge_metadata::from_json(json!({
            "version": 3, "sources": [],
            "actions": [{"name": "bad", "definition": {"output_type": "String",
                "handler": "http://h", "response_transform": {"version": 3}}}]
        }))
        .unwrap_err();
        assert_eq!(e.path, "$.actions[0].definition.response_transform.version");

        // Kriti templates are only checked when the action runs.
        let metadata = hge_metadata::from_json(json!({
            "version": 3, "sources": [],
            "actions": [
                {"name": "good", "definition": {"output_type": "String", "handler": "http://u:p@h/x",
                    "request_transform": {"body": "{{ unparsed"},
                    "response_transform": {"version": 2, "body": {"action": "remove"}}}},
                {"name": "plain", "definition": {"output_type": "String", "handler": "http://h"}}
            ]
        }))
        .unwrap();
        let output = Arc::new(ActionOutput {
            ty: OutputType::Named("String".into()),
            base: OutputBase::BuiltinScalar,
        });
        let outputs = ["good", "plain"]
            .into_iter()
            .map(|n| (n.to_owned(), output.clone()))
            .collect();
        let built = ActionRegistry::build(
            &metadata,
            &outputs,
            Arc::new(crate::ProcessEnv),
            HandlerConfig::default(),
        );
        assert!(built.failed.is_empty() && built.inconsistencies.is_empty());
        let good = built.registry.get("good").unwrap();
        assert!(good.request_transform.is_some() && good.response_transform.is_some());
        let base = good.transform_base.clone().unwrap().unwrap();
        assert_eq!(base.full_url(), "http://h/x");
        assert_eq!(base.headers[0].0, "Authorization");
        assert!(
            built
                .registry
                .get("plain")
                .unwrap()
                .transform_base
                .is_none()
        );
    }
}
