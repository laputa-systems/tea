//! Deterministic prompt-cacheability measurements at the model request boundary.
//!
//! A provider cache hit is provider-specific and must be reported from provider usage. The
//! measurements here are deliberately narrower: they describe how much of two adjacent
//! [`ModelRequest`] values is byte-identical before transport serialization. Hosts can use this
//! as a cacheability proxy and pair it with `Usage::cache_read_tokens` when a provider reports
//! real cache accounting.

use crate::scheduler::{AdapterRequestObservation, ModelRequest};
use tea_protocol::JsonValue;

/// Content-free deterministic comparison of adjacent logical provider inputs.
///
/// This is deliberately separate from provider cache usage: it measures the
/// core-owned request surface before any provider-specific transport envelope
/// is applied and therefore never claims a cache hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeterministicPrefixEvidence {
    /// Longest shared byte prefix of the two canonical logical requests.
    pub common_prefix_bytes: u64,
    /// Stable rough token estimate for the shared logical prefix.
    pub common_prefix_tokens_estimate: u64,
}

/// Measure the exact shared prefix of two adjacent logical provider requests.
///
/// `None` means no preceding request exists in this core run. A returned zero
/// is meaningful evidence that a predecessor existed but shared no bytes.
pub fn deterministic_request_prefix_evidence(
    previous: Option<&ModelRequest>,
    current: &ModelRequest,
) -> Option<DeterministicPrefixEvidence> {
    let previous = previous?;
    let previous = canonical_request_surface_bytes(previous);
    let current = canonical_request_surface_bytes(current);
    let common_prefix_bytes = common_prefix_len(&previous, &current) as u64;
    Some(DeterministicPrefixEvidence {
        common_prefix_bytes,
        // This is explicitly an estimate. It avoids claiming a tokenizer the
        // provider may not expose while keeping adjacent traces comparable.
        common_prefix_tokens_estimate: common_prefix_bytes.saturating_add(3) / 4,
    })
}

/// The source of cache accounting attached to a request observation.
///
/// A matching byte prefix is useful diagnostic evidence, but it never proves
/// that a provider read from a prompt cache. Only provider usage can do that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAccountingStatus {
    /// The provider supplied cache-read or cache-write token accounting.
    ProviderReported,
    /// Tea measured a comparable request prefix but has no provider accounting.
    PrefixProxy,
    /// Neither provider accounting nor a meaningful predecessor comparison exists.
    Unavailable,
}

/// Byte-oriented comparison of one request with its predecessor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptCacheMeasurement {
    /// Stable fingerprint for system prompt, ordered tools, model, and thinking level.
    pub cache_domain_fingerprint: u64,
    /// Whether the predecessor belongs to the same prompt/cache domain.
    pub cache_domain_changed: bool,
    /// System-prompt byte length for the current request.
    pub system_prompt_bytes: usize,
    /// Deterministic ordered tool-definition byte length for the current request.
    pub tool_definition_bytes: usize,
    /// Converted provider-context byte length for the current request.
    pub context_bytes: usize,
    /// Approximate full prompt bytes before a provider-specific envelope is added.
    pub prompt_bytes: usize,
    /// Longest common byte prefix of adjacent converted provider contexts.
    ///
    /// This is a deterministic cacheability proxy, not a provider cache hit.
    pub common_context_prefix_bytes: usize,
    /// Longest common prefix as millionths of the predecessor context length.
    pub common_context_prefix_ratio_millionths: u32,
    /// Stable fingerprint of the current converted provider context.
    pub context_fingerprint: u64,
    /// Fingerprint of the current system prompt.
    pub system_prompt_fingerprint: u64,
    /// Fingerprint of the ordered complete tool definitions.
    pub tool_definition_fingerprint: u64,
    /// Fingerprint of tool names in their exposed order.
    pub tool_order_fingerprint: u64,
    /// Fingerprint of the selected model identity.
    pub model_fingerprint: u64,
    /// Fingerprint of the provider-neutral reasoning configuration.
    pub thinking_fingerprint: u64,
    /// Whether the current context is an exact extension of its predecessor.
    pub exact_context_extension: bool,
    /// Whether the only context change is an append to the predecessor.
    ///
    /// This is a request-layout proxy, not evidence that a provider cache was used.
    pub append_only_context: bool,
    /// Optional byte count of the exact adapter-serialized request.
    pub adapter_serialized_request_bytes: Option<usize>,
    /// Optional adapter-defined cache-domain fingerprint.
    pub adapter_cache_domain_fingerprint: Option<u64>,
    /// Component names whose normalized fingerprints changed from the predecessor.
    pub changed_cache_domain_components: Vec<String>,
}

/// Compare a request with an optional immediately preceding request.
pub fn measure_prompt_cacheability(
    previous: Option<&ModelRequest>,
    current: &ModelRequest,
) -> PromptCacheMeasurement {
    measure_request_layout(previous, current, None, None)
}

/// Compare the exact core request and optional observations produced by the adapter that sent it.
///
/// The caller supplies observations captured from the same preparation/send path. This function
/// is pure and intentionally cannot invoke hooks, project context, rebuild tools, or serialize a
/// second request.
pub fn measure_request_layout(
    previous: Option<&ModelRequest>,
    current: &ModelRequest,
    previous_adapter: Option<&AdapterRequestObservation>,
    current_adapter: Option<&AdapterRequestObservation>,
) -> PromptCacheMeasurement {
    let current_tools = tool_definition_bytes(current);
    let current_domain = cache_domain_fingerprint(current, &current_tools);
    let same_domain = previous
        .map(|request| {
            cache_domain_fingerprint(request, &tool_definition_bytes(request)) == current_domain
        })
        .unwrap_or(false);
    let common_context_prefix_bytes = previous
        .filter(|_| same_domain)
        .map(|request| common_prefix_len(request.context.as_bytes(), current.context.as_bytes()))
        .unwrap_or(0);
    let previous_context_bytes = previous.map_or(0, |request| request.context.len());
    let common_context_prefix_ratio_millionths = if previous_context_bytes == 0 {
        0
    } else {
        ((common_context_prefix_bytes as u128 * 1_000_000) / previous_context_bytes as u128)
            .min(u32::MAX as u128) as u32
    };
    let exact_context_extension = previous
        .is_some_and(|request| same_domain && common_context_prefix_bytes == request.context.len());
    let mut changed_cache_domain_components = Vec::new();
    if let Some(previous) = previous {
        let previous_tools = tool_definition_bytes(previous);
        if stable_fingerprint(previous.system_prompt.as_bytes())
            != stable_fingerprint(current.system_prompt.as_bytes())
        {
            changed_cache_domain_components.push("system_prompt".into());
        }
        if stable_fingerprint(&previous_tools) != stable_fingerprint(&current_tools) {
            changed_cache_domain_components.push("tool_definitions".into());
        }
        if tool_order_fingerprint(previous) != tool_order_fingerprint(current) {
            changed_cache_domain_components.push("tool_order".into());
        }
        if model_fingerprint(previous) != model_fingerprint(current) {
            changed_cache_domain_components.push("model".into());
        }
        if thinking_fingerprint(previous) != thinking_fingerprint(current) {
            changed_cache_domain_components.push("thinking".into());
        }
        compare_adapter_components(
            previous_adapter,
            current_adapter,
            &mut changed_cache_domain_components,
        );
    }
    PromptCacheMeasurement {
        cache_domain_fingerprint: current_domain,
        cache_domain_changed: previous.is_some() && !same_domain,
        system_prompt_bytes: current.system_prompt.len(),
        tool_definition_bytes: current_tools.len(),
        context_bytes: current.context.len(),
        prompt_bytes: current
            .system_prompt
            .len()
            .saturating_add(current_tools.len())
            .saturating_add(current.context.len()),
        common_context_prefix_bytes,
        common_context_prefix_ratio_millionths,
        context_fingerprint: stable_fingerprint(current.context.as_bytes()),
        system_prompt_fingerprint: stable_fingerprint(current.system_prompt.as_bytes()),
        tool_definition_fingerprint: stable_fingerprint(&current_tools),
        tool_order_fingerprint: tool_order_fingerprint(current),
        model_fingerprint: model_fingerprint(current),
        thinking_fingerprint: thinking_fingerprint(current),
        exact_context_extension,
        append_only_context: exact_context_extension,
        adapter_serialized_request_bytes: current_adapter
            .and_then(|observation| observation.serialized_request_bytes),
        adapter_cache_domain_fingerprint: current_adapter
            .and_then(|observation| observation.cache_domain_fingerprint),
        changed_cache_domain_components,
    }
}

fn cache_domain_fingerprint(request: &ModelRequest, tools: &[u8]) -> u64 {
    let mut bytes = Vec::with_capacity(
        request
            .system_prompt
            .len()
            .saturating_add(tools.len())
            .saturating_add(64),
    );
    bytes.extend_from_slice(request.system_prompt.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(tools);
    bytes.push(0);
    if let Some(model) = &request.model {
        bytes.extend_from_slice(model.provider.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(model.model.as_bytes());
        bytes.push(0);
        if let Some(revision) = &model.revision {
            bytes.extend_from_slice(revision.as_bytes());
        }
    }
    bytes.push(0);
    bytes.extend_from_slice(format!("{:?}", request.thinking_level).as_bytes());
    stable_fingerprint(&bytes)
}

fn tool_order_fingerprint(request: &ModelRequest) -> u64 {
    let mut bytes = Vec::new();
    for tool in &request.tools {
        bytes.extend_from_slice(tool.name.as_bytes());
        bytes.push(0);
    }
    stable_fingerprint(&bytes)
}

fn model_fingerprint(request: &ModelRequest) -> u64 {
    let mut bytes = Vec::new();
    if let Some(model) = &request.model {
        bytes.extend_from_slice(model.provider.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(model.model.as_bytes());
        bytes.push(0);
        if let Some(revision) = &model.revision {
            bytes.extend_from_slice(revision.as_bytes());
        }
    }
    stable_fingerprint(&bytes)
}

fn thinking_fingerprint(request: &ModelRequest) -> u64 {
    stable_fingerprint(format!("{:?}", request.thinking_level).as_bytes())
}

fn compare_adapter_components(
    previous: Option<&AdapterRequestObservation>,
    current: Option<&AdapterRequestObservation>,
    changed: &mut Vec<String>,
) {
    let (Some(previous), Some(current)) = (previous, current) else {
        return;
    };
    if previous.cache_domain_fingerprint != current.cache_domain_fingerprint {
        changed.push("adapter_cache_domain".into());
    }
    for name in previous
        .cache_domain_components
        .keys()
        .chain(current.cache_domain_components.keys())
    {
        if previous.cache_domain_components.get(name) != current.cache_domain_components.get(name) {
            let label = format!("adapter.{name}");
            if !changed.contains(&label) {
                changed.push(label);
            }
        }
    }
}

fn tool_definition_bytes(request: &ModelRequest) -> Vec<u8> {
    let definitions = request
        .tools
        .iter()
        .map(|tool| {
            JsonValue::object([
                ("name", JsonValue::from(tool.name.clone())),
                ("description", JsonValue::from(tool.description.clone())),
                ("schema", tool.schema.clone()),
                (
                    "execution_mode",
                    JsonValue::from(format!("{:?}", tool.execution_mode)),
                ),
            ])
        })
        .collect::<Vec<_>>();
    JsonValue::Array(definitions)
        .to_json_string()
        .unwrap_or_default()
        .into_bytes()
}

fn canonical_request_surface_bytes(request: &ModelRequest) -> Vec<u8> {
    let tools = tool_definition_bytes(request);
    let mut bytes = Vec::with_capacity(
        request
            .system_prompt
            .len()
            .saturating_add(tools.len())
            .saturating_add(request.context.len())
            .saturating_add(128),
    );
    bytes.extend_from_slice(b"tea-core-request-surface-v1\0");
    append_length_delimited(&mut bytes, request.system_prompt.as_bytes());
    append_length_delimited(&mut bytes, &tools);
    match &request.model {
        Some(model) => {
            bytes.push(1);
            append_length_delimited(&mut bytes, model.provider.as_bytes());
            append_length_delimited(&mut bytes, model.model.as_bytes());
            match &model.revision {
                Some(revision) => {
                    bytes.push(1);
                    append_length_delimited(&mut bytes, revision.as_bytes());
                }
                None => bytes.push(0),
            }
        }
        None => bytes.push(0),
    }
    append_length_delimited(
        &mut bytes,
        format!("{:?}", request.thinking_level).as_bytes(),
    );
    // Context deliberately occupies the tail: an append-only context change
    // then preserves an equally append-only canonical request prefix.
    bytes.extend_from_slice(b"context\0");
    bytes.extend_from_slice(request.context.as_bytes());
    bytes
}

fn append_length_delimited(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_le_bytes());
    output.extend_from_slice(value);
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn stable_fingerprint(bytes: &[u8]) -> u64 {
    // FNV-1a is small, deterministic, and sufficient for a diagnostic fingerprint. It is not
    // used as an identity, authorization token, or cryptographic digest.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::ModelRequest;
    use crate::state::{ModelDescriptor, ThinkingLevel};

    fn request(context: &str) -> ModelRequest {
        ModelRequest {
            system_prompt: "system".into(),
            context: context.into(),
            tools: Vec::new(),
            model: Some(ModelDescriptor {
                provider: "fixture".into(),
                model: "model".into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::Off,
        }
    }

    #[test]
    fn reports_common_context_prefix_without_calling_it_a_hit() {
        let previous = request("[one]");
        let current = request("[one]{\"role\":\"user\"}");
        let measurement = measure_prompt_cacheability(Some(&previous), &current);
        assert_eq!(
            measurement.common_context_prefix_bytes,
            previous.context.len()
        );
        assert_eq!(
            measurement.common_context_prefix_ratio_millionths,
            1_000_000
        );
        assert!(!measurement.cache_domain_changed);
    }

    #[test]
    fn domain_changes_zero_the_reusable_prefix() {
        let previous = request("[one]");
        let mut current = request("[one]");
        current.system_prompt = "changed".into();
        let measurement = measure_prompt_cacheability(Some(&previous), &current);
        assert_eq!(measurement.common_context_prefix_bytes, 0);
        assert!(measurement.cache_domain_changed);
    }

    #[test]
    fn deterministic_prefix_counts_the_stable_surface_and_appended_context_tail() {
        let previous = request("[one]");
        let current = request("[one][two]");
        let evidence = deterministic_request_prefix_evidence(Some(&previous), &current)
            .expect("a predecessor produces deterministic evidence");
        assert!(evidence.common_prefix_bytes > previous.context.len() as u64);
        assert_eq!(
            evidence.common_prefix_tokens_estimate,
            evidence.common_prefix_bytes.saturating_add(3) / 4
        );
        assert_eq!(deterministic_request_prefix_evidence(None, &current), None);
    }
}
