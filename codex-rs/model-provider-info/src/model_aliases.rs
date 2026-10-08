//! Share a runtime catalog between configured aliases of the same backend.

use crate::ModelProviderInfo;
use std::collections::HashMap;

impl ModelProviderInfo {
    /// Aliases must agree on transport, credentials, and every overlapping model's
    /// routing and metadata. Only explicit reasoning levels may differ.
    pub fn can_share_model_session(&self, other: &Self) -> bool {
        if self.base_url.is_none() || self.models.is_empty() || other.models.is_empty() {
            return false;
        }
        let mut left = self.clone();
        let mut right = other.clone();
        left.models.clear();
        right.models.clear();
        left.model_reasoning_efforts.clear();
        right.model_reasoning_efforts.clear();
        if left != right {
            return false;
        }
        self.models.iter().all(|model| {
            other
                .models
                .iter()
                .filter(|other| other.id == model.id)
                .all(|other| {
                    // An omitted effort is a model default, not an explicit variant.
                    if model.reasoning_effort.is_some() != other.reasoning_effort.is_some() {
                        return false;
                    }
                    let mut left = model.clone();
                    let mut right = other.clone();
                    left.name = None;
                    right.name = None;
                    left.reasoning_effort = None;
                    right.reasoning_effort = None;
                    left == right
                })
        })
    }

    /// Keep this provider's defaults while making compatible aliases' models and
    /// explicit reasoning levels available to the existing runtime client.
    pub fn with_model_aliases(&self, providers: &HashMap<String, Self>) -> Self {
        let mut runtime = self.clone();
        let mut aliases = providers.iter().collect::<Vec<_>>();
        aliases.sort_by_key(|(id, _)| *id);
        for (_, alias) in aliases {
            if !runtime.can_share_model_session(alias) {
                continue;
            }
            for model in &alias.models {
                if let Some(existing) = runtime.models.iter().find(|entry| entry.id == model.id) {
                    if let (Some(default), Some(effort)) =
                        (&existing.reasoning_effort, &model.reasoning_effort)
                        && default != effort
                    {
                        let efforts = runtime
                            .model_reasoning_efforts
                            .entry(model.id.clone())
                            .or_insert_with(|| vec![default.clone()]);
                        if !efforts.contains(effort) {
                            efforts.push(effort.clone());
                        }
                    }
                } else {
                    runtime.models.push(model.clone());
                }
            }
        }
        runtime
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ReasoningEffort;

    fn aliases() -> (ModelProviderInfo, ModelProviderInfo) {
        let primary: ModelProviderInfo = toml::from_str(
            r#"
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
[[models]]
id = "flash"
context_window = 1048576
reasoning_effort = "max"
openrouter_providers = ["together"]
openrouter_zdr = true
[[models]]
id = "glm"
context_window = 65536
reasoning_effort = "high"
openrouter_providers = ["wafer"]
openrouter_zdr = true
"#,
        )
        .unwrap();
        let mut high = primary.clone();
        high.models.truncate(1);
        high.models[0].reasoning_effort = Some(ReasoningEffort::High);
        (primary, high)
    }

    #[test]
    fn equivalent_aliases_share_models_and_reasoning_in_both_directions() {
        let (primary, high) = aliases();
        let providers = HashMap::from([
            ("openrouter".into(), primary.clone()),
            ("openrouter-high".into(), high.clone()),
        ]);
        for provider in [&primary, &high] {
            let runtime = provider.with_model_aliases(&providers);
            runtime.validate().unwrap();
            assert_eq!(runtime.models.len(), 2);
            assert_eq!(runtime.models[0], provider.models[0]);
            let efforts = &runtime.model_reasoning_efforts["flash"];
            assert!(efforts.contains(&ReasoningEffort::High));
            assert!(efforts.contains(&ReasoningEffort::Max));
            assert_eq!(runtime.models[1].openrouter_providers, ["wafer"]);
            assert!(
                !toml::to_string(&runtime)
                    .unwrap()
                    .contains("model_reasoning_efforts")
            );
        }
        assert_eq!(high.models.len(), 1, "picker catalogs remain unchanged");
    }

    #[test]
    fn different_transport_credentials_or_model_routing_cannot_share_a_session() {
        let (primary, high) = aliases();
        for difference in [
            "url",
            "key",
            "header",
            "capabilities",
            "context",
            "routing",
            "zdr",
            "omitted-effort",
        ] {
            let mut other = high.clone();
            match difference {
                "url" => other.base_url = Some("https://api.isoquant.ai/v1".into()),
                "key" => other.env_key = Some("OTHER_KEY".into()),
                "header" => {
                    other.http_headers = Some(HashMap::from([("x-account".into(), "other".into())]))
                }
                "capabilities" => other.supports_standalone_web_search = true,
                "context" => other.models[0].context_window = 32768,
                "routing" => other.models[0].openrouter_providers = vec!["other".into()],
                "zdr" => other.models[0].openrouter_zdr = Some(false),
                "omitted-effort" => other.models[0].reasoning_effort = None,
                _ => unreachable!(),
            }
            assert!(!primary.can_share_model_session(&other), "{difference}");
            let providers = HashMap::from([("other".into(), other)]);
            assert_eq!(
                primary.with_model_aliases(&providers),
                primary,
                "{difference}"
            );
        }
    }
}
