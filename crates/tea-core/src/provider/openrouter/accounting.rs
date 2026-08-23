//! OpenRouter cost and usage accounting contracts.

use crate::state::Usage;

/// Origin of one OpenRouter cost record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterCostSource {
    /// The Chat Completions response supplied `usage.cost` directly.
    ChatUsage,
    /// A follow-up OpenRouter generation lookup supplied richer accounting metadata.
    Generation,
    /// OpenRouter reported neither usable chat nor generation accounting.
    Unavailable,
}

impl OpenRouterCostSource {
    /// Stable JSON/report spelling for this source.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatUsage => "openrouter_chat_usage",
            Self::Generation => "openrouter_generation",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Redacted, provider-reported cost for one model turn.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenRouterCostTurn {
    /// One-based model-turn sequence in the provider instance.
    pub turn: usize,
    /// Accounting response that supplied this record.
    pub source: OpenRouterCostSource,
    /// Provider-reported total USD cost, if available.
    pub total_usd: Option<f64>,
    /// Exact non-negative decimal token from the provider response.
    ///
    /// This is the authoritative representation for accounting. `total_usd` is inherently
    /// lossy for decimal provider prices.
    pub total_usd_exact: Option<String>,
    /// Provider-reported upstream inference USD cost, if available.
    pub upstream_inference_usd: Option<f64>,
    /// Exact provider decimal for upstream inference cost, when supplied.
    pub upstream_inference_usd_exact: Option<String>,
    /// Concrete provider model, when OpenRouter supplied it.
    pub model: Option<String>,
    /// OpenRouter-selected provider name, when generation metadata supplied it.
    pub provider: Option<String>,
    /// Provider-reported input token count, if available.
    pub input_tokens: Option<u64>,
    /// Provider-reported output token count, if available.
    pub output_tokens: Option<u64>,
    /// Provider-reported cache-read token count, if available.
    pub cache_read_tokens: Option<u64>,
    /// Provider-reported cache-write token count, if available.
    pub cache_write_tokens: Option<u64>,
    /// Provider-reported reasoning token count, if available.
    pub reasoning_tokens: Option<u64>,
}

/// Snapshot of redacted provider accounting for all completed turns.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenRouterCostReport {
    /// Number of turns for which OpenRouter reported a total price.
    pub reported_turn_count: usize,
    /// Number of turns without provider accounting.
    pub unavailable_turn_count: usize,
    /// Whether every completed turn has provider-reported cost.
    pub complete: bool,
    /// Sum of reported total USD values. See [`Self::complete`] before treating it as a run total.
    pub reported_total_usd: f64,
    /// Exact decimal sum of all reported total prices, without floating-point rounding.
    pub reported_total_usd_exact: Option<String>,
    /// Sum of reported upstream inference USD values where supplied.
    pub reported_upstream_inference_usd: f64,
    /// Exact decimal sum of all reported upstream inference prices.
    pub reported_upstream_inference_usd_exact: Option<String>,
    /// Per-turn accounting records without request text, response IDs, raw payloads, or secrets.
    pub turns: Vec<OpenRouterCostTurn>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Accounting {
    pub(super) usage: Usage,
    pub(super) costs: Vec<OpenRouterCostTurn>,
}

pub(super) fn add_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}
