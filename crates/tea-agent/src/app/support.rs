use tea_core::state::{ThinkingLevel, Usage};

/// Format only provider-reported values; missing fields remain absent rather than zero.
pub fn format_usage(usage: &Usage) -> String {
    let mut fields = Vec::new();
    if let Some(value) = usage.input_tokens {
        fields.push(format!("in {value}"));
    }
    if let Some(value) = usage.output_tokens {
        fields.push(format!("out {value}"));
    }
    if let Some(value) = usage.reasoning_tokens {
        fields.push(format!("reasoning {value}"));
    }
    if let Some(value) = usage.cache_read_tokens {
        fields.push(format!("cache-read {value}"));
    }
    if let Some(value) = usage.cache_write_tokens {
        fields.push(format!("cache-write {value}"));
    }
    if let Some(value) = usage.cost.as_deref() {
        fields.push(format!("cost {value}"));
    }
    if fields.is_empty() {
        "provider reported no accounting".into()
    } else {
        fields.join(", ")
    }
}

pub(super) fn format_compact_tokens(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 10_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else if value < 1_000_000 {
        format!("{}k", (value + 500) / 1_000)
    } else if value < 10_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else {
        format!("{}M", (value + 500_000) / 1_000_000)
    }
}

pub(super) const fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

pub(super) const fn thinking_levels() -> [ThinkingLevel; 7] {
    [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ]
}

pub(super) fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    Some(match value {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        "max" => ThinkingLevel::Max,
        _ => return None,
    })
}
