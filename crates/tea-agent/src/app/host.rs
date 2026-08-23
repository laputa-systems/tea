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
pub(super) fn host_configuration(tools: TeaCodingToolsV2) -> Result<AgentConfiguration, AppError> {
    let profile = TeaDefaultCodingProfileV2::pinned_default()
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let registry = tools.registry();
    profile
        .validate_registry(&registry)
        .map_err(|error| AppError::Setup(error.to_string()))?;
    Ok(AgentConfiguration::new(
        profile.system_prompt_for_workspace(tools.workspace().as_path()),
        registry,
        Arc::new(OpenAiContextHook),
    ))
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
