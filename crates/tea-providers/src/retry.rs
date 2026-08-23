//! Bounded retry policy shared by the finite-response provider adapters.

use crate::scheduler::CancellationToken;
use std::time::Duration;

/// Bounded exponential-backoff policy for replay-safe provider attempts.
///
/// `max_retries` is in addition to the first attempt. The default retries a transient failure
/// three times with delays of 250 ms, 500 ms, and 1 s, capped at 8 s for callers that choose a
/// larger retry budget. The policy is used only before a provider stream has exposed an event;
/// replaying a partially consumed stream remains the embedding's responsibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_retries: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl RetryPolicy {
    /// Construct a policy. A maximum delay below the initial delay is raised to the initial
    /// delay, keeping the backoff monotonic without introducing a fallible configuration path.
    pub fn new(max_retries: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_retries,
            initial_delay,
            max_delay: if max_delay < initial_delay {
                initial_delay
            } else {
                max_delay
            },
        }
    }

    /// Return the default bounded policy used by built-in adapters.
    pub const fn standard() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(8),
        }
    }

    /// Number of retries after the initial attempt.
    pub const fn max_retries(self) -> u32 {
        self.max_retries
    }

    /// Initial delay before the first retry.
    pub const fn initial_delay(self) -> Duration {
        self.initial_delay
    }

    /// Maximum delay between attempts.
    pub const fn max_delay(self) -> Duration {
        self.max_delay
    }

    /// Calculate the delay before retry number `retry_index`, where zero is the first retry.
    pub fn delay_before_retry(self, retry_index: u32) -> Duration {
        let mut delay = self.initial_delay;
        for _ in 0..retry_index {
            delay = delay.checked_mul(2).unwrap_or(self.max_delay);
            if delay >= self.max_delay {
                return self.max_delay;
            }
        }
        delay.min(self.max_delay)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// A one-attempt failure with an adapter-local retry classification.
pub(crate) struct RetryableError {
    pub(crate) message: String,
    pub(crate) retryable: bool,
}

/// Run a replay-safe operation with bounded exponential backoff.
pub(crate) fn retry_with_backoff<T, F>(
    policy: RetryPolicy,
    cancellation: &CancellationToken,
    mut operation: F,
) -> Result<T, String>
where
    F: FnMut() -> Result<T, RetryableError>,
{
    let mut retry_index = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err("provider request cancelled".into());
        }
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if !error.retryable || retry_index >= policy.max_retries => {
                return Err(error.message);
            }
            Err(_) => {
                let delay = policy.delay_before_retry(retry_index);
                retry_index += 1;
                if !wait_with_cancellation(delay, cancellation) {
                    return Err("provider request cancelled".into());
                }
            }
        }
    }
}

/// Wait between attempts without making cancellation wait for the full backoff interval.
pub(crate) fn wait_with_cancellation(delay: Duration, cancellation: &CancellationToken) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < delay {
        if cancellation.is_cancelled() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10).min(delay.saturating_sub(started.elapsed())));
    }
    !cancellation.is_cancelled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_policy_is_exponential_and_capped() {
        let policy = RetryPolicy::standard();
        assert_eq!(policy.delay_before_retry(0), Duration::from_millis(250));
        assert_eq!(policy.delay_before_retry(1), Duration::from_millis(500));
        assert_eq!(policy.delay_before_retry(2), Duration::from_secs(1));
        assert_eq!(policy.delay_before_retry(10), Duration::from_secs(8));
    }

    #[test]
    fn retries_transient_attempts_then_returns_success() {
        let policy = RetryPolicy::new(2, Duration::ZERO, Duration::ZERO);
        let cancellation = CancellationToken::new();
        let mut attempts = 0;
        let result = retry_with_backoff(policy, &cancellation, || {
            attempts += 1;
            if attempts < 3 {
                Err(RetryableError {
                    message: "transient".into(),
                    retryable: true,
                })
            } else {
                Ok("ok")
            }
        });
        assert_eq!(result, Ok("ok"));
        assert_eq!(attempts, 3);
    }

    #[cfg(feature = "provider-openrouter")]
    #[test]
    fn permanent_failure_does_not_retry() {
        let policy = RetryPolicy::new(3, Duration::ZERO, Duration::ZERO);
        let cancellation = CancellationToken::new();
        let mut attempts = 0;
        let result: Result<(), _> = retry_with_backoff(policy, &cancellation, || {
            attempts += 1;
            Err(RetryableError {
                message: "invalid request".into(),
                retryable: false,
            })
        });
        assert_eq!(result, Err("invalid request".into()));
        assert_eq!(attempts, 1);
    }
}
