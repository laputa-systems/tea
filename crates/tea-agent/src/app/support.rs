use std::time::{Duration, SystemTime, UNIX_EPOCH};
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

/// Format today's UTC civil date without adding a date/time dependency.
pub(super) fn utc_date() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's public-domain civil-date conversion, expressed locally to
// keep Command Code host metadata explicit without a time crate.
pub(super) fn civil_from_days(days_since_unix_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}
