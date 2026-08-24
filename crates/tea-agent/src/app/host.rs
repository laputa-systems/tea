use std::sync::Arc;
use tea_core::agent::AgentConfiguration;
use tea_core::coding::{TeaCodingToolsV2, TeaDefaultCodingProfileV2};
use tea_providers::{openai::OpenAiContextHook, ModelDescriptor, ProviderRegistry};

use super::error::AppError;

/// Assemble the immutable configuration that each durable terminal epoch receives.
///
/// The terminal intentionally never constructs an unmanaged [`tea_core::Agent`]. Its only
/// execution authority is the session-owned durable harness, which captures this configuration
/// in a committed revision before starting an epoch.
pub(super) fn host_configuration(
    tools: TeaCodingToolsV2,
    logical_workspace_label: &str,
) -> Result<AgentConfiguration, AppError> {
    let profile = TeaDefaultCodingProfileV2::pinned_default()
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let registry = tools.registry();
    profile
        .validate_registry(&registry)
        .map_err(|error| AppError::Setup(error.to_string()))?;
    Ok(AgentConfiguration::new(
        profile.system_prompt_for_workspace(std::path::Path::new(logical_workspace_label)),
        registry,
        Arc::new(OpenAiContextHook),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_configuration_keeps_the_logical_workspace_outside_tool_authority() {
        let physical = std::env::temp_dir().join(format!(
            "tea-agent-host-logical-workspace-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&physical).expect("physical workspace creates");
        let configuration = host_configuration(
            TeaCodingToolsV2::new(&physical).expect("physical tools configure"),
            "/stable/logical/workspace",
        )
        .expect("host configuration assembles");

        assert!(configuration
            .system_prompt
            .contains("Current working directory: /stable/logical/workspace"));
        assert!(
            !configuration
                .system_prompt
                .contains(&*physical.to_string_lossy()),
            "the prompt must not disclose the authority-bearing physical path"
        );
        let _ = std::fs::remove_dir_all(physical);
    }

    #[test]
    fn physical_child_worktree_spelling_does_not_change_logical_prompt_fingerprint() {
        let root = std::env::temp_dir().join(format!(
            "tea-agent-host-logical-fingerprint-{}",
            std::process::id()
        ));
        let first_physical = root.join("child-a");
        let second_physical = root.join("child-b");
        std::fs::create_dir_all(&first_physical).expect("first physical worktree creates");
        std::fs::create_dir_all(&second_physical).expect("second physical worktree creates");
        let first = host_configuration(
            TeaCodingToolsV2::new(&first_physical).expect("first child tools configure"),
            "/stable/logical/workspace",
        )
        .expect("first child configuration assembles");
        let second = host_configuration(
            TeaCodingToolsV2::new(&second_physical).expect("second child tools configure"),
            "/stable/logical/workspace",
        )
        .expect("second child configuration assembles");
        let first_request = tea_core::scheduler::ModelRequest {
            system_prompt: first.system_prompt,
            context: "stable child assignment".into(),
            ..tea_core::scheduler::ModelRequest::default()
        };
        let second_request = tea_core::scheduler::ModelRequest {
            system_prompt: second.system_prompt,
            context: "stable child assignment".into(),
            ..tea_core::scheduler::ModelRequest::default()
        };
        let measurement = tea_core::measurement::measure_prompt_cacheability(
            Some(&first_request),
            &second_request,
        );

        assert_eq!(
            measurement.cache_domain_fingerprint,
            tea_core::measurement::measure_prompt_cacheability(None, &first_request)
                .cache_domain_fingerprint
        );
        assert!(!measurement.cache_domain_changed);
        assert!(
            !second_request
                .system_prompt
                .contains(&*first_physical.to_string_lossy())
                && !second_request
                    .system_prompt
                    .contains(&*second_physical.to_string_lossy())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ModelCandidate {
    pub(super) provider: &'static str,
    pub(super) provider_name: &'static str,
    pub(super) model: Option<ModelDescriptor>,
}

const MOCK_MODEL: ModelDescriptor = ModelDescriptor {
    id: super::mock::DEFAULT_MODEL_ID,
    display_name: "Safe TUI playground",
    context_window: Some(super::mock::CONTEXT_WINDOW),
};

impl ModelCandidate {
    pub(super) fn label(self) -> String {
        match self.model {
            Some(model) => format!("{} · {}", self.provider_name, model.id),
            None => format!("{} · custom model…", self.provider_name),
        }
    }

    pub(super) fn model_id(self) -> Option<&'static str> {
        self.model.map(|model| model.id)
    }
}

pub(super) fn model_candidates(registry: &ProviderRegistry, filter: &str) -> Vec<ModelCandidate> {
    let filter = filter.to_ascii_lowercase();
    let mut candidates = Vec::new();
    for entry in registry.providers() {
        for model in entry.models {
            if model.id.to_ascii_lowercase().contains(&filter)
                || model.display_name.to_ascii_lowercase().contains(&filter)
                || entry.id.to_ascii_lowercase().contains(&filter)
                || entry.display_name.to_ascii_lowercase().contains(&filter)
            {
                candidates.push(ModelCandidate {
                    provider: entry.id,
                    provider_name: entry.display_name,
                    model: Some(*model),
                });
            }
        }
        if entry.allows_custom_model()
            && ("custom model".contains(&filter)
                || entry.id.to_ascii_lowercase().contains(&filter)
                || entry.display_name.to_ascii_lowercase().contains(&filter))
        {
            candidates.push(ModelCandidate {
                provider: entry.id,
                provider_name: entry.display_name,
                model: None,
            });
        }
    }
    if "mock".contains(&filter) || "safe tui playground".contains(&filter) {
        candidates.push(ModelCandidate {
            provider: super::mock::PROVIDER_ID,
            provider_name: "Mock",
            model: Some(MOCK_MODEL),
        });
    }
    candidates
}

pub(super) fn overlay_lines(
    title: &str,
    filter: &str,
    candidates: &[String],
    selected: usize,
    max_rows: usize,
) -> Vec<String> {
    let mut lines = vec![if filter.is_empty() {
        format!("{title} {} · Type to filter", candidates.len())
    } else {
        format!("{title} {} · {filter}", candidates.len())
    }];
    if candidates.is_empty() {
        lines.push("  No matching models".into());
    } else {
        let visible = max_rows.saturating_sub(2).max(1).min(candidates.len());
        let start = selected
            .saturating_sub(visible.saturating_sub(1))
            .min(candidates.len().saturating_sub(visible));
        lines.extend(candidates[start..start + visible].iter().enumerate().map(
            |(offset, candidate)| {
                let index = start + offset;
                format!("{} {candidate}", if index == selected { '❯' } else { ' ' })
            },
        ));
    }
    lines.push("↑/↓ navigate · Enter select · Esc close".into());
    lines
}
