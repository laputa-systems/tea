//! Compact Anthropic model metadata.
//!
//! The flags follow upstream Pi's catalog generator rules
//! (`scripts/generate-models.ts`: `supportsAnthropicMidConvoEffort`,
//! `supportsAnthropicMidConvoSystemMessages`, `isAnthropicAdaptiveThinkingModel`,
//! `isAnthropicTemperatureUnsupportedModel`, `ANTHROPIC_PROMPT_CACHE`) for the
//! few models Tea lists. It is deliberately not an exhaustive catalog. Prices
//! are listed only where they are well established; an unknown price stays
//! unknown and disables price-based decisions such as cache warming.

/// Behavior switches for one Anthropic Messages model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnthropicCompat {
    /// The model can think at all.
    pub reasoning: bool,
    /// Thinking is adaptive (effort-based) rather than budget-based.
    pub force_adaptive_thinking: bool,
    /// Effort is carried as per-turn mid-conversation configuration markers
    /// with drop-on-mismatch thinking binding.
    pub mid_conversation_effort: bool,
    /// Later system messages are accepted in place.
    pub mid_conversation_system_messages: bool,
    /// Tool additions and removals are accepted in place.
    pub mid_conversation_tool_changes: bool,
    /// `thinking: {type: "disabled"}` is accepted.
    pub thinking_can_be_disabled: bool,
    /// Native `xhigh` effort.
    pub native_xhigh: bool,
    /// Native `max` effort.
    pub native_max: bool,
    /// A sampling temperature may be sent.
    pub supports_temperature: bool,
    /// The one-hour cache lifetime may be requested.
    pub supports_long_cache_retention: bool,
    /// Tool input is streamed eagerly per tool rather than through the
    /// fine-grained tool-streaming beta.
    pub eager_tool_input_streaming: bool,
    /// A cache breakpoint may be placed on the last tool.
    pub cache_control_on_tools: bool,
    /// Unsigned thinking may be replayed with an empty signature.
    pub allow_empty_signature: bool,
}

impl AnthropicCompat {
    /// Conservative switches for a caller-selected model: no thinking, no
    /// in-place configuration, standard caching. Nothing is inferred from the
    /// model name.
    pub const fn conservative() -> Self {
        Self {
            reasoning: false,
            force_adaptive_thinking: false,
            mid_conversation_effort: false,
            mid_conversation_system_messages: false,
            mid_conversation_tool_changes: false,
            thinking_can_be_disabled: false,
            native_xhigh: false,
            native_max: false,
            supports_temperature: true,
            supports_long_cache_retention: true,
            eager_tool_input_streaming: true,
            cache_control_on_tools: true,
            allow_empty_signature: false,
        }
    }

    /// Switches of the managed-effort Claude 5 generation (Opus 5.5, Sonnet
    /// 5.5, Fable 5.1).
    const fn managed_claude_5() -> Self {
        Self {
            reasoning: true,
            force_adaptive_thinking: true,
            mid_conversation_effort: true,
            mid_conversation_system_messages: true,
            mid_conversation_tool_changes: true,
            thinking_can_be_disabled: false,
            native_xhigh: true,
            native_max: true,
            supports_temperature: false,
            supports_long_cache_retention: true,
            eager_tool_input_streaming: true,
            cache_control_on_tools: true,
            allow_empty_signature: false,
        }
    }

    const fn budget_thinking() -> Self {
        Self {
            reasoning: true,
            thinking_can_be_disabled: true,
            ..Self::conservative()
        }
    }
}

/// Exact token prices in decimal dollars per million tokens, as listed.
///
/// One-hour cache writes are billed at twice the input price.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListedPricing {
    /// Uncached input.
    pub input: &'static str,
    /// Output.
    pub output: &'static str,
    /// Cache reads.
    pub cache_read: &'static str,
    /// Five-minute cache writes.
    pub cache_write: &'static str,
}

impl ListedPricing {
    /// Owned provider-neutral prices.
    pub fn to_pricing(self) -> crate::scheduler::ModelPricing {
        crate::scheduler::ModelPricing {
            input: self.input.into(),
            output: self.output.into(),
            cache_read: self.cache_read.into(),
            cache_write: self.cache_write.into(),
        }
    }
}

/// Prompt-cache lifetimes for direct Anthropic models (Pi's `ANTHROPIC_PROMPT_CACHE`).
pub const SHORT_CACHE_TTL_SECONDS: u64 = 300;
/// One-hour cache lifetime.
pub const LONG_CACHE_TTL_SECONDS: u64 = 3_600;

/// One listed model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnthropicModel {
    /// Model identifier.
    pub id: &'static str,
    /// Picker name.
    pub display_name: &'static str,
    /// Context capacity in tokens.
    pub context_window: u64,
    /// Default output-token ceiling.
    pub max_output_tokens: u32,
    /// Behavior switches.
    pub compat: AnthropicCompat,
    /// Prices, when established.
    pub pricing: Option<ListedPricing>,
}

/// The listed models.
pub const MODELS: &[AnthropicModel] = &[
    AnthropicModel {
        id: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        context_window: 200_000,
        max_output_tokens: 32_000,
        compat: AnthropicCompat::managed_claude_5(),
        pricing: None,
    },
    AnthropicModel {
        id: "claude-sonnet-5-5",
        display_name: "Claude Sonnet 5.5",
        context_window: 200_000,
        max_output_tokens: 32_000,
        compat: AnthropicCompat::managed_claude_5(),
        pricing: None,
    },
    AnthropicModel {
        id: "claude-fable-5-1",
        display_name: "Claude Fable 5.1",
        context_window: 200_000,
        max_output_tokens: 32_000,
        compat: AnthropicCompat {
            // Fable models accept a sampling temperature outside managed
            // effort; managed effort never sends one.
            supports_temperature: true,
            ..AnthropicCompat::managed_claude_5()
        },
        pricing: None,
    },
    AnthropicModel {
        id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output_tokens: 64_000,
        compat: AnthropicCompat::budget_thinking(),
        pricing: Some(ListedPricing {
            input: "1",
            output: "5",
            cache_read: "0.1",
            cache_write: "1.25",
        }),
    },
];

/// Find a listed model.
pub fn model(id: &str) -> Option<&'static AnthropicModel> {
    MODELS.iter().find(|model| model.id == id)
}
