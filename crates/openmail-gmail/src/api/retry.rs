//! Exponential backoff with jitter for retried requests.

use std::time::Duration;

/// How the client retries rate-limited (429), quota-limited and server-error
/// (5xx) responses, and timed-out requests.
///
/// The delay before retry `n` (counting from 0) is drawn uniformly from
/// `[d/2, d]`, where `d = initial_backoff * 2^n` capped at `max_backoff`. If Gmail
/// sends `Retry-After`, the client waits at least that long, up to `max_backoff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt. `0` disables retries.
    pub max_retries: u32,
    /// The base delay before the first retry.
    pub initial_backoff: Duration,
    /// The longest delay between two attempts.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(32),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries.
    pub const fn none() -> Self {
        Self {
            max_retries: 0,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        }
    }

    /// The delay before retry `attempt` (0 for the first retry).
    pub fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self
            .initial_backoff
            .saturating_mul(2_u32.saturating_pow(attempt))
            .min(self.max_backoff);
        let half = exp / 2;
        let jitter = Duration::from_nanos(fastrand::u64(
            0..=u64::try_from((exp - half).as_nanos()).unwrap_or(u64::MAX),
        ));
        let delay = half + jitter;
        retry_after.map_or(delay, |wait| delay.max(wait).min(self.max_backoff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_within_jitter_bounds() {
        let policy = RetryPolicy {
            max_retries: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
        };
        for _ in 0..200 {
            let d0 = policy.backoff(0, None);
            assert!((Duration::from_millis(50)..=Duration::from_millis(100)).contains(&d0));
            let d2 = policy.backoff(2, None);
            assert!((Duration::from_millis(200)..=Duration::from_millis(400)).contains(&d2));
            let capped = policy.backoff(30, None);
            assert!((Duration::from_millis(500)..=Duration::from_secs(1)).contains(&capped));
        }
    }

    #[test]
    fn jitter_varies_the_delay() {
        let policy = RetryPolicy::default();
        let delays: std::collections::HashSet<_> =
            (0..50).map(|_| policy.backoff(3, None)).collect();
        assert!(delays.len() > 1);
    }

    #[test]
    fn retry_after_is_respected_up_to_the_cap() {
        let policy = RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_secs(5),
        };
        assert_eq!(
            policy.backoff(0, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        assert_eq!(
            policy.backoff(0, Some(Duration::from_secs(60))),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn no_retry_policy_has_zero_delay() {
        assert_eq!(RetryPolicy::none().backoff(4, None), Duration::ZERO);
    }
}
