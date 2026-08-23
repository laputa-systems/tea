//! Explicit model/tool profiles.
//!
//! The pinned Pi coding profile is checked-in captured data. This module provides its stable
//! shape without pretending that tool names, prompt text, or schemas can be safely copied from
//! memory. A host may instead provide a sterile profile.

use crate::error::ProfileError;
use crate::tool::{ToolDefinition, ToolRegistry};
use std::collections::BTreeMap;
use std::path::Path;
use tea_protocol::JsonValue;

const PINNED_DEFAULT_PROFILE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/profile/default-profile.json"
));

/// Prompt and ordered tool specification for one agent profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfileSpec {
    /// Ordered system prompt text.
    pub system_prompt: String,
    /// Ordered prompt-facing tool definitions.
    pub tools: Vec<ToolDefinition>,
    /// Tool-local prompt guidance in source order.
    pub tool_guidance: Vec<String>,
}

impl ProfileSpec {
    /// Validate the profile's stable ordering invariants.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let mut names = std::collections::BTreeSet::new();
        for tool in &self.tools {
            if tool.name.trim().is_empty() {
                return Err(ProfileError::new("profile tool name cannot be empty"));
            }
            if !names.insert(&tool.name) {
                return Err(ProfileError::new(format!(
                    "duplicate profile tool: {}",
                    tool.name
                )));
            }
        }
        Ok(())
    }
}

/// An explicit, host-owned profile used to configure an agent.
#[derive(Clone, Debug, PartialEq)]
pub struct PiDefaultCodingProfile {
    spec: ProfileSpec,
    standard_tools: Vec<ToolDefinition>,
    captured_workspace_root: Option<String>,
}

impl PiDefaultCodingProfile {
    /// Construct from a captured profile specification.
    pub fn from_spec(spec: ProfileSpec) -> Result<Self, ProfileError> {
        spec.validate()?;
        Ok(Self {
            standard_tools: spec.tools.clone(),
            spec,
            captured_workspace_root: None,
        })
    }

    /// Load the exact checked-in default profile capture.
    ///
    /// The capture includes the rendered default prompt, active tool definitions, and the
    /// complete standard tool inventory. It is compiled into the crate, so constructing this
    /// profile never reads a cwd, home directory, or live Pi installation.
    pub fn pinned_default() -> Result<Self, ProfileError> {
        let capture = JsonValue::parse(PINNED_DEFAULT_PROFILE).map_err(|error| {
            ProfileError::new(format!("invalid pinned profile capture: {error}"))
        })?;
        let root = profile_object(&capture, "profile capture")?;
        if profile_number(root, "format_version")? != 1
            || profile_string(root, "kind")? != "pinned_default_coding_profile"
        {
            return Err(ProfileError::new(
                "pinned profile capture has an unsupported format or kind",
            ));
        }
        let prompt = profile_object(profile_field(root, "system_prompt")?, "system_prompt")?;
        let inputs = profile_object(profile_field(root, "inputs")?, "inputs")?;
        let spec = ProfileSpec {
            system_prompt: profile_string(prompt, "text")?.to_owned(),
            tools: parse_profile_tools(profile_field(root, "active_tools")?)?,
            // The capture's system prompt is already composed with these exact
            // snippets and guidelines. Appending them here would duplicate prompt text.
            tool_guidance: Vec::new(),
        };
        spec.validate()?;
        Ok(Self {
            standard_tools: parse_profile_tools(profile_field(root, "standard_tools")?)?,
            spec,
            captured_workspace_root: Some(profile_string(inputs, "workspace_root")?.to_owned()),
        })
    }

    /// Borrow the captured specification.
    pub fn spec(&self) -> &ProfileSpec {
        &self.spec
    }

    /// Render the profile's ordered system prompt and tool-local guidance.
    pub fn system_prompt(&self) -> String {
        let mut prompt = self.spec.system_prompt.clone();
        for guidance in &self.spec.tool_guidance {
            if !prompt.is_empty() {
                prompt.push('\n');
            }
            prompt.push_str(guidance);
        }
        prompt
    }

    /// Render the captured prompt for one explicit workspace authority.
    ///
    /// The profile fixture fixes its workspace at capture time so prompt bytes
    /// can be verified. The runtime must not leak that fixture path into a
    /// model request: this method performs only that declared substitution,
    /// after the caller has supplied and canonicalized a workspace. Profiles
    /// constructed with [`Self::from_spec`] have no captured placeholder and
    /// return their prompt unchanged.
    pub fn system_prompt_for_workspace(&self, workspace: &Path) -> String {
        let prompt = self.system_prompt();
        let Some(captured_workspace_root) = &self.captured_workspace_root else {
            return prompt;
        };
        let workspace = workspace.to_string_lossy().replace('\\', "/");
        prompt.replace(captured_workspace_root, &workspace)
    }

    /// Apply this profile's active tools to a registry by name while preserving registry
    /// replacement/removal policy.  Executable implementations are always supplied by caller.
    pub fn active_tool_names(&self) -> impl Iterator<Item = &str> {
        self.spec.tools.iter().map(|tool| tool.name.as_str())
    }

    /// Return definitions for inspection or protocol conversion.
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        &self.spec.tools
    }

    /// Return all standard tool definitions captured in the default profile.
    ///
    /// This includes standard but inactive tools. [`Self::tool_definitions`] contains only the
    /// exact default active set used by the system prompt.
    pub fn standard_tool_definitions(&self) -> &[ToolDefinition] {
        &self.standard_tools
    }

    /// Verify that every captured profile tool has a host implementation.
    pub fn validate_registry(&self, registry: &ToolRegistry) -> Result<(), ProfileError> {
        for tool in &self.spec.tools {
            if registry.get(&tool.name).is_none() {
                return Err(ProfileError::new(format!(
                    "profile tool is not registered: {}",
                    tool.name
                )));
            }
        }
        Ok(())
    }
}

/// A deliberately empty profile for applications that supply all prompt/tools themselves.
pub fn sterile_profile() -> ProfileSpec {
    ProfileSpec::default()
}

fn parse_profile_tools(value: &JsonValue) -> Result<Vec<ToolDefinition>, ProfileError> {
    profile_array(value, "profile tools")?
        .iter()
        .map(|tool| {
            let tool = profile_object(tool, "profile tool")?;
            Ok(ToolDefinition {
                name: profile_string(tool, "name")?.to_owned(),
                description: profile_string(tool, "description")?.to_owned(),
                schema: profile_field(tool, "parameters")?.clone(),
                // Pi's `Agent` default is parallel; definitions with no explicit mode inherit it.
                execution_mode: crate::tool::ToolExecutionMode::Parallel,
            })
        })
        .collect()
}

fn profile_field<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<&'a JsonValue, ProfileError> {
    object
        .get(name)
        .ok_or_else(|| ProfileError::new(format!("pinned profile is missing {name:?}")))
}

fn profile_object<'a>(
    value: &'a JsonValue,
    path: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, ProfileError> {
    match value {
        JsonValue::Object(object) => Ok(object),
        _ => Err(ProfileError::new(format!(
            "pinned profile {path} must be an object"
        ))),
    }
}

fn profile_array<'a>(value: &'a JsonValue, path: &str) -> Result<&'a [JsonValue], ProfileError> {
    match value {
        JsonValue::Array(values) => Ok(values),
        _ => Err(ProfileError::new(format!(
            "pinned profile {path} must be an array"
        ))),
    }
}

fn profile_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<&'a str, ProfileError> {
    match profile_field(object, name)? {
        JsonValue::String(value) => Ok(value),
        _ => Err(ProfileError::new(format!(
            "pinned profile field {name:?} must be a string"
        ))),
    }
}

fn profile_number(object: &BTreeMap<String, JsonValue>, name: &str) -> Result<u64, ProfileError> {
    match profile_field(object, name)? {
        JsonValue::Number(tea_protocol::JsonNumber::Unsigned(value)) => Ok(*value),
        JsonValue::Number(tea_protocol::JsonNumber::Signed(value)) if *value >= 0 => {
            Ok(*value as u64)
        }
        _ => Err(ProfileError::new(format!(
            "pinned profile field {name:?} must be a non-negative integer"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::PiDefaultCodingProfile;
    use std::path::Path;

    #[test]
    fn pinned_default_uses_the_captured_prompt_and_active_tool_order() {
        let profile = PiDefaultCodingProfile::pinned_default().expect("pinned capture is valid");

        assert!(
            profile
                .system_prompt()
                .starts_with("You are an expert coding assistant operating inside pi")
        );
        assert_eq!(
            profile.active_tool_names().collect::<Vec<_>>(),
            ["read", "bash", "edit", "write"]
        );
        assert_eq!(
            profile
                .standard_tool_definitions()
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "bash", "edit", "write", "grep", "find", "ls"]
        );
    }

    #[test]
    fn pinned_default_substitutes_only_the_explicit_capture_workspace() {
        let profile = PiDefaultCodingProfile::pinned_default().expect("pinned capture is valid");
        let prompt = profile.system_prompt_for_workspace(Path::new("/explicit/workspace"));

        assert!(prompt.contains("Current working directory: /explicit/workspace"));
        assert!(!prompt.contains("Current working directory: /fixture/workspace"));
    }
}
