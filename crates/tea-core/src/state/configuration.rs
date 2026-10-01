//! Model-visible configuration carried in conversation order.
//!
//! A run's instructions and declared tools are not a single value attached to
//! every request. They have a position in the transcript: the first
//! [`AgentMessage::System`] declares the initial configuration and every later
//! one changes it from that point onward. Replaying these messages in order
//! yields the configuration in force at any message, which is what keeps
//! reopen, fork, compaction, and provider projection from applying a later
//! configuration to earlier history.
//!
//! This is the Rust form of upstream Pi's leading/later `SystemMessage`
//! contract (`packages/ai/src/utils/transcript.ts`). Tea expresses prompt text
//! only as named sections; Pi's free-form `content` append has no Tea caller.

use super::AgentMessage;
use crate::tool::ToolDeclaration;
use std::fmt;

/// Maximum UTF-8 bytes in one prompt-section identity.
pub const MAX_PROMPT_SECTION_ID_BYTES: usize = 200;

/// One named model-visible system-prompt section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSection {
    /// Stable section identity used to relate a later update to this section.
    pub id: String,
    /// Exact section text.
    pub content: String,
}

impl PromptSection {
    /// Construct one named section.
    pub fn new(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
        }
    }
}

/// The ordered sectioned system prompt selected for future requests.
///
/// The rendered prompt joins non-empty sections with a blank line, matching
/// the harness composition rule. Section identities are unique.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemPrompt {
    sections: Vec<PromptSection>,
}

/// Default section identity for a prompt supplied as one text value.
pub const DEFAULT_PROMPT_SECTION_ID: &str = "system";

impl SystemPrompt {
    /// Construct a prompt from ordered, uniquely named sections.
    pub fn new(sections: Vec<PromptSection>) -> Result<Self, ConfigurationError> {
        let mut seen = std::collections::BTreeSet::new();
        for section in &sections {
            validate_section_id(&section.id)?;
            if !seen.insert(section.id.as_str()) {
                return Err(ConfigurationError::DuplicateSection {
                    id: section.id.clone(),
                });
            }
        }
        Ok(Self { sections })
    }

    /// A prompt consisting of one default-named section.
    pub fn single(text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            return Self::default();
        }
        Self {
            sections: vec![PromptSection::new(DEFAULT_PROMPT_SECTION_ID, text)],
        }
    }

    /// Borrow the ordered sections.
    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
    }

    /// Render the complete prompt text.
    pub fn render(&self) -> String {
        render_sections(&self.sections)
    }
}

impl From<String> for SystemPrompt {
    fn from(value: String) -> Self {
        Self::single(value)
    }
}

impl From<&str> for SystemPrompt {
    fn from(value: &str) -> Self {
        Self::single(value)
    }
}

impl From<&String> for SystemPrompt {
    fn from(value: &String) -> Self {
        Self::single(value.clone())
    }
}

/// One section change at a configuration update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SectionChange {
    /// Section identity.
    pub id: String,
    /// Replacement text, or `None` to remove the section.
    pub content: Option<String>,
}

/// A model-visible configuration change at one point in conversation order.
///
/// On the first system message of a transcript this is the initial
/// configuration: every section and tool is an addition. Later updates are
/// deltas. A changed tool definition is a removal followed by an addition, so
/// transports that reference tools by name can detect redefinition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigurationUpdate {
    /// Section replacements, additions, and removals in application order.
    pub sections: Vec<SectionChange>,
    /// Complete declarations of tools that become available here.
    pub tools_added: Vec<ToolDeclaration>,
    /// Names of tools that stop being available here.
    pub tools_removed: Vec<String>,
}

impl ConfigurationUpdate {
    /// Whether this update changes nothing.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty() && self.tools_added.is_empty() && self.tools_removed.is_empty()
    }

    /// Approximate model-visible bytes of this update.
    pub fn approximate_bytes(&self) -> usize {
        let sections = self
            .sections
            .iter()
            .map(|change| change.id.len() + change.content.as_ref().map_or(0, String::len))
            .sum::<usize>();
        let added = self
            .tools_added
            .iter()
            .map(|tool| {
                tool.name.len()
                    + tool.description.len()
                    + tool.schema.to_json_string().map_or(0, |schema| schema.len())
            })
            .sum::<usize>();
        let removed = self.tools_removed.iter().map(String::len).sum::<usize>();
        sections + added + removed
    }

    /// Validate structural bounds before the update can become durable.
    pub fn validate(&self) -> Result<(), ConfigurationError> {
        let mut seen = std::collections::BTreeSet::new();
        for change in &self.sections {
            validate_section_id(&change.id)?;
            if !seen.insert(change.id.as_str()) {
                return Err(ConfigurationError::DuplicateSection {
                    id: change.id.clone(),
                });
            }
        }
        let mut added = std::collections::BTreeSet::new();
        for tool in &self.tools_added {
            if tool.name.is_empty() {
                return Err(ConfigurationError::EmptyToolName);
            }
            if !added.insert(tool.name.as_str()) {
                return Err(ConfigurationError::DuplicateTool {
                    name: tool.name.clone(),
                });
            }
        }
        let mut removed = std::collections::BTreeSet::new();
        for name in &self.tools_removed {
            if name.is_empty() {
                return Err(ConfigurationError::EmptyToolName);
            }
            if !removed.insert(name.as_str()) {
                return Err(ConfigurationError::DuplicateTool { name: name.clone() });
            }
        }
        Ok(())
    }

    /// Render this update for a transport that accepts system text in place.
    ///
    /// Section changes are framed by identity so the model can relate them to
    /// the leading prompt. This framing is request-time only, as in Pi's
    /// `renderSystemMessageUpdate`.
    pub fn render_update_text(&self) -> String {
        let mut parts = Vec::new();
        for change in &self.sections {
            parts.push(match &change.content {
                Some(content) => format!(
                    "Updated system prompt section \"{}\":\n\n{content}",
                    change.id
                ),
                None => format!("Removed system prompt section \"{}\".", change.id),
            });
        }
        parts.join("\n\n")
    }
}

/// The configuration in force after replaying system messages in order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectiveConfiguration {
    /// Sections in replay order: an existing section is replaced in place and
    /// a new section is appended.
    pub sections: Vec<PromptSection>,
    /// Declared tools in replay order: a newly added tool is appended.
    pub tools: Vec<ToolDeclaration>,
}

impl EffectiveConfiguration {
    /// Construct the desired configuration for one request.
    pub fn new(prompt: &SystemPrompt, tools: Vec<ToolDeclaration>) -> Self {
        Self {
            sections: prompt.sections().to_vec(),
            tools,
        }
    }

    /// Apply one update in transcript order.
    pub fn apply(&mut self, update: &ConfigurationUpdate) {
        for change in &update.sections {
            let position = self
                .sections
                .iter()
                .position(|section| section.id == change.id);
            match (&change.content, position) {
                (Some(content), Some(index)) => self.sections[index].content = content.clone(),
                (Some(content), None) => self
                    .sections
                    .push(PromptSection::new(change.id.clone(), content.clone())),
                (None, Some(index)) => {
                    self.sections.remove(index);
                }
                (None, None) => {}
            }
        }
        for name in &update.tools_removed {
            self.tools.retain(|tool| &tool.name != name);
        }
        for tool in &update.tools_added {
            self.tools.retain(|existing| existing.name != tool.name);
            self.tools.push(tool.clone());
        }
    }

    /// Replay every system message, returning `None` when none exists.
    pub fn replay<'a>(messages: impl IntoIterator<Item = &'a AgentMessage>) -> Option<Self> {
        let mut configuration: Option<Self> = None;
        for message in messages {
            if let AgentMessage::System { update, .. } = message {
                configuration.get_or_insert_with(Self::default).apply(update);
            }
        }
        configuration
    }

    /// Render the effective system prompt.
    pub fn system_prompt(&self) -> String {
        render_sections(&self.sections)
    }

    /// Find one declared tool.
    pub fn tool(&self, name: &str) -> Option<&ToolDeclaration> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// Represent this configuration as the initial system message.
    pub fn as_initial_update(&self) -> ConfigurationUpdate {
        ConfigurationUpdate {
            sections: self
                .sections
                .iter()
                .map(|section| SectionChange {
                    id: section.id.clone(),
                    content: Some(section.content.clone()),
                })
                .collect(),
            tools_added: self.tools.clone(),
            tools_removed: Vec::new(),
        }
    }

    /// Whether `other` declares the same sections and tools, ignoring order.
    ///
    /// Replay appends new sections and tools, so a desired configuration in
    /// composition order can equal a replayed one in a different order. Order
    /// alone never produces an update; that keeps the provider prefix stable.
    pub fn same_content(&self, other: &Self) -> bool {
        self.sections.len() == other.sections.len()
            && self.tools.len() == other.tools.len()
            && self.sections.iter().all(|section| {
                other
                    .sections
                    .iter()
                    .any(|candidate| candidate == section)
            })
            && self
                .tools
                .iter()
                .all(|tool| other.tool(&tool.name) == Some(tool))
    }

    /// Compute the update that turns this configuration into `desired`.
    ///
    /// Returns `None` when both declare the same content.
    pub fn diff(&self, desired: &Self) -> Option<ConfigurationUpdate> {
        if self.same_content(desired) {
            return None;
        }
        let mut sections = Vec::new();
        for section in &desired.sections {
            let current = self
                .sections
                .iter()
                .find(|candidate| candidate.id == section.id);
            if current.map(|current| &current.content) != Some(&section.content) {
                sections.push(SectionChange {
                    id: section.id.clone(),
                    content: Some(section.content.clone()),
                });
            }
        }
        for section in &self.sections {
            if !desired
                .sections
                .iter()
                .any(|candidate| candidate.id == section.id)
            {
                sections.push(SectionChange {
                    id: section.id.clone(),
                    content: None,
                });
            }
        }
        let tools_removed = self
            .tools
            .iter()
            .filter(|tool| desired.tool(&tool.name) != Some(tool))
            .map(|tool| tool.name.clone())
            .collect();
        let tools_added = desired
            .tools
            .iter()
            .filter(|tool| self.tool(&tool.name) != Some(tool))
            .cloned()
            .collect();
        Some(ConfigurationUpdate {
            sections,
            tools_added,
            tools_removed,
        })
    }
}

fn render_sections(sections: &[PromptSection]) -> String {
    sections
        .iter()
        .map(|section| section.content.as_str())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn validate_section_id(id: &str) -> Result<(), ConfigurationError> {
    if id.is_empty() || id.len() > MAX_PROMPT_SECTION_ID_BYTES || id.chars().any(char::is_control)
    {
        return Err(ConfigurationError::InvalidSectionId { id: id.to_owned() });
    }
    Ok(())
}

/// Invalid configuration material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigurationError {
    /// A section identity is empty, too long, or contains control characters.
    InvalidSectionId {
        /// Rejected identity.
        id: String,
    },
    /// Two sections share one identity.
    DuplicateSection {
        /// Duplicated identity.
        id: String,
    },
    /// A tool declaration or removal names no tool.
    EmptyToolName,
    /// One update adds or removes the same tool twice.
    DuplicateTool {
        /// Duplicated tool name.
        name: String,
    },
}

impl fmt::Display for ConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSectionId { id } => {
                write!(formatter, "invalid system prompt section identity {id:?}")
            }
            Self::DuplicateSection { id } => {
                write!(formatter, "duplicate system prompt section {id:?}")
            }
            Self::EmptyToolName => formatter.write_str("configuration names an empty tool"),
            Self::DuplicateTool { name } => {
                write!(formatter, "configuration names tool {name:?} twice")
            }
        }
    }
}

impl std::error::Error for ConfigurationError {}
