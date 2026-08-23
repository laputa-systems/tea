//! Frozen session-level self-extension exposure modes.
//!
//! A mode is selected by the trusted host when creating a session. It is not a
//! Luau-editable setting and never becomes dynamic prompt prose; the stable
//! addendum's presence and the fixed host-tool registry are the only
//! model-visible consequences.

use tea_protocol::JsonValue;

/// Immutable session-header metadata key for the selected mode.
pub const SELF_EXTENSION_MODE_METADATA_KEY: &str = "tea.self_extension_mode";

/// Trusted metadata placed on an accepted user entry only when the host has
/// explicitly routed that request through its authoring affordance. It is not
/// inferred from prompt text: doing so would let a model manufacture its own
/// authorization by paraphrasing a user request.
pub const AUTHORING_AUTHORIZATION_METADATA_KEY: &str = "tea.authoring_authorized";

/// Exact versioned concise prompt artifact used by enabled profiles.
pub const SELF_EXTENSION_V1_CONCISE: &str = "Session harness self-extension\n\nYou may improve Tea's session-local harness only when repeated evidence or a clearly reusable failure indicates a harness problem. Do not create a plugin for one-off task facts or ordinary implementation work. Prefer the smallest change and preserve unrelated behavior.\n\nUse `tea_harness` to inspect or atomically edit Luau plugins. A plugin is a closed directory containing `manifest.json` and its declared `.luau` modules. The manifest names the entrypoint and every module. The entrypoint returns named prompt sections and may declare bounded hooks or capability-neutral tools. Imports must be relative and declared. Plugins have no ambient filesystem, process, network, environment, session-storage, evaluator, or capability-grant access. Use `tea_harness` with `operation: \"help\"` for the complete ABI.\n\nAfter an edit, Tea validates and snapshots the complete harness automatically. A valid snapshot activates only at a safe run boundary; Tea then continues the task under the new snapshot. Failure leaves the previous snapshot active. Never issue or wait for a reload command.";

/// Trusted session-level policy for exposing self-extension capabilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelfExtensionMode {
    /// No authoring prompt or `tea_harness` model capability.
    Off,
    /// The model may author only in response to an explicit user request.
    Author,
    /// The model may stage bounded reusable repairs under the rollover budget.
    Adaptive,
}

impl Default for SelfExtensionMode {
    fn default() -> Self {
        Self::Off
    }
}

impl SelfExtensionMode {
    /// Stable durable spelling suitable for session metadata and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Author => "author",
            Self::Adaptive => "adaptive",
        }
    }

    /// Parse only the three fixed durable spellings.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "author" => Some(Self::Author),
            "adaptive" => Some(Self::Adaptive),
            _ => None,
        }
    }

    /// Whether the stable host control capability may be presented.
    pub const fn exposes_control_tool(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether a mutation must be tied to a host-recorded user authorization.
    pub const fn requires_explicit_user_authorization(self) -> bool {
        matches!(self, Self::Author)
    }

    /// Return the immutable metadata representation expected in a new session
    /// header. The host owns header construction, so this helper cannot modify
    /// an existing session.
    pub fn metadata_value(self) -> JsonValue {
        JsonValue::String(self.as_str().into())
    }
}
