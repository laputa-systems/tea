//! Provider usage and exact decimal accounting.

use super::*;

/// Provider usage counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    /// Input tokens, when reported by the provider.
    pub input_tokens: Option<u64>,
    /// Output tokens, when reported by the provider.
    pub output_tokens: Option<u64>,
    /// Reasoning tokens, when reported by the provider.
    pub reasoning_tokens: Option<u64>,
    /// Input tokens served from a provider cache, when reported.
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to a provider cache, when reported.
    pub cache_write_tokens: Option<u64>,
    /// Exact provider-reported monetary value for this response, when reported.
    ///
    /// This is retained as the provider's decimal text rather than an `f64`, so a host can
    /// display or add reported prices without binary floating-point rounding. The core never
    /// derives a value from token counts or a pricing table.
    pub cost: Option<String>,
}

impl Usage {
    /// Whether this usage value contains at least one provider-reported field.
    pub fn is_reported(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.reasoning_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_tokens.is_some()
            || self.cost.is_some()
    }

    /// Merge a later provider update into this value without turning unknown fields into zero.
    pub fn merge(&mut self, update: Self) {
        if update.input_tokens.is_some() {
            self.input_tokens = update.input_tokens;
        }
        if update.output_tokens.is_some() {
            self.output_tokens = update.output_tokens;
        }
        if update.reasoning_tokens.is_some() {
            self.reasoning_tokens = update.reasoning_tokens;
        }
        if update.cache_read_tokens.is_some() {
            self.cache_read_tokens = update.cache_read_tokens;
        }
        if update.cache_write_tokens.is_some() {
            self.cache_write_tokens = update.cache_write_tokens;
        }
        if update.cost.is_some() {
            self.cost = update.cost;
        }
    }

    /// Accumulate one provider report into session totals without turning unknown fields into
    /// zero. Reported decimal costs are added exactly as decimal text.
    pub fn accumulate(&mut self, update: Self) {
        add_usage(&mut self.input_tokens, update.input_tokens);
        add_usage(&mut self.output_tokens, update.output_tokens);
        add_usage(&mut self.reasoning_tokens, update.reasoning_tokens);
        add_usage(&mut self.cache_read_tokens, update.cache_read_tokens);
        add_usage(&mut self.cache_write_tokens, update.cache_write_tokens);
        if let Some(cost) = update.cost.as_deref() {
            self.cost = Some(match self.cost.as_deref() {
                Some(previous) => decimal_add(previous, cost),
                None => cost.to_owned(),
            });
        }
    }
}

/// Accounting attached to one settled model turn.
///
/// `run_id` and `turn_id` identify the exact response that produced the report. `model` is the
/// provider-independent request identity, which remains available even when a provider's
/// response does not echo its model name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelTurnAccounting {
    /// Run that owns this model turn.
    pub run_id: RunId,
    /// Turn within the run.
    pub turn_id: TurnId,
    /// Model requested for this turn, when configured.
    pub model: Option<ModelDescriptor>,
    /// Provider-reported token and monetary fields.
    pub usage: Usage,
}

/// Retained per-turn and aggregate model accounting for an agent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelAccountingSnapshot {
    /// One record for each model turn that supplied a usage update.
    pub turns: Vec<ModelTurnAccounting>,
    /// Field-wise aggregate of known provider reports. A field remains `None` until at least
    /// one turn reports it; reported zero remains `Some(0)`.
    pub aggregate: Usage,
}

impl ModelAccountingSnapshot {
    /// Record one settled model-turn report and update its aggregate view.
    pub(crate) fn record(&mut self, accounting: ModelTurnAccounting) {
        add_usage(
            &mut self.aggregate.input_tokens,
            accounting.usage.input_tokens,
        );
        add_usage(
            &mut self.aggregate.output_tokens,
            accounting.usage.output_tokens,
        );
        add_usage(
            &mut self.aggregate.reasoning_tokens,
            accounting.usage.reasoning_tokens,
        );
        add_usage(
            &mut self.aggregate.cache_read_tokens,
            accounting.usage.cache_read_tokens,
        );
        add_usage(
            &mut self.aggregate.cache_write_tokens,
            accounting.usage.cache_write_tokens,
        );
        if let Some(cost) = accounting.usage.cost.as_deref() {
            self.aggregate.cost = Some(match self.aggregate.cost.as_deref() {
                Some(previous) => decimal_add(previous, cost),
                None => cost.to_owned(),
            });
        }
        self.turns.push(accounting);
    }
}

fn add_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

/// Add two non-negative decimal strings without converting through `f64`.
fn decimal_add(lhs: &str, rhs: &str) -> String {
    let (left_digits, left_scale) = decimal_parts(lhs);
    let (right_digits, right_scale) = decimal_parts(rhs);
    let scale = left_scale.max(right_scale);
    let mut left = left_digits;
    let mut right = right_digits;
    left.extend(std::iter::repeat_n('0', scale - left_scale));
    right.extend(std::iter::repeat_n('0', scale - right_scale));
    let mut output = String::new();
    let mut carry = 0u8;
    let mut left = left.bytes().rev();
    let mut right = right.bytes().rev();
    loop {
        let left = left.next();
        let right = right.next();
        if left.is_none() && right.is_none() {
            break;
        }
        let sum = left.unwrap_or(b'0') - b'0' + right.unwrap_or(b'0') - b'0' + carry;
        output.push(char::from(b'0' + sum % 10));
        carry = sum / 10;
    }
    if carry != 0 {
        output.push(char::from(b'0' + carry));
    }
    let mut output: String = output.chars().rev().collect();
    if scale != 0 {
        if output.len() <= scale {
            let zeros = "0".repeat(scale + 1 - output.len());
            output = format!("{zeros}{output}");
        }
        output.insert(output.len() - scale, '.');
    }
    decimal_normalize(&output)
}

fn decimal_parts(value: &str) -> (String, usize) {
    let (coefficient, exponent) = value
        .split_once(['e', 'E'])
        .map(|(coefficient, exponent)| (coefficient, exponent.parse::<i64>().unwrap_or(0)))
        .unwrap_or((value, 0));
    let (whole, fraction) = coefficient.split_once('.').unwrap_or((coefficient, ""));
    let mut digits = String::with_capacity(whole.len() + fraction.len());
    digits.push_str(whole.trim_start_matches('+'));
    digits.push_str(fraction);
    let scale = (fraction.len() as i64 - exponent).max(0) as usize;
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        digits.push('0');
    }
    (digits, scale)
}

fn decimal_normalize(value: &str) -> String {
    let (digits, scale) = decimal_parts(value);
    if scale == 0 {
        return digits;
    }
    let mut output = if digits.len() <= scale {
        format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
    } else {
        let position = digits.len() - scale;
        format!("{}.{}", &digits[..position], &digits[position..])
    };
    while output.ends_with('0') {
        output.pop();
    }
    if output.ends_with('.') {
        output.pop();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::Usage;

    #[test]
    fn accumulation_adds_reported_fields_without_filling_unknowns() {
        let mut total = Usage {
            input_tokens: Some(2),
            cost: Some("0.20".into()),
            ..Usage::default()
        };
        total.accumulate(Usage {
            input_tokens: Some(3),
            output_tokens: Some(4),
            cost: Some("0.005".into()),
            ..Usage::default()
        });
        assert_eq!(total.input_tokens, Some(5));
        assert_eq!(total.output_tokens, Some(4));
        assert_eq!(total.reasoning_tokens, None);
        assert_eq!(total.cost.as_deref(), Some("0.205"));
    }
}
