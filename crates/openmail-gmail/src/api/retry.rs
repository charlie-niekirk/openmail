//! Exponential backoff with jitter for retried requests.

use std::time::Duration;

/// How the client retries rate-limited (429), quota-limited and server-error
/// (5xx) responses, and timed-out requests.
///
/// The delay before retry `n` (counting from 0) is drawn uniformly from
/// `[d/2, d]`, where `d = initial_backoff * 2^n` capped at `max_backoff`. If Gmail
/// sends `Retry-After`, the client waits at least that long, up to `max_backoff`.
///
/// Rate limits (429 and rate-limit 403s) are retried on their own budget, since
/// Gmail's per-user limit is a moving average over a minute and retrying
/// quickly only keeps the user over it: delays start at
/// `rate_limit_initial_backoff` and grow the same way up to
/// `rate_limit_max_backoff`, and retries continue until the delays add up to
/// `rate_limit_max_wait`. An exhausted daily quota is never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries of server errors and timeouts after the first attempt. `0`
    /// disables them.
    pub max_retries: u32,
    /// The base delay before the first retry.
    pub initial_backoff: Duration,
    /// The longest delay between two attempts after a server error or timeout.
    pub max_backoff: Duration,
    /// The base delay before the first retry after a rate limit.
    pub rate_limit_initial_backoff: Duration,
    /// The longest delay between two attempts after a rate limit.
    pub rate_limit_max_backoff: Duration,
    /// The most time spent waiting on rate limits for one request before
    /// giving up with [`Error::Quota`](super::Error::Quota). `ZERO` disables
    /// rate-limit retries.
    pub rate_limit_max_wait: Duration,
}

impl Default for RetryPolicy {
    /// Five retries up to 32 s apart for server errors; up to three minutes of
    /// rate-limit backoff, starting at 5 s and at most a minute at a time.
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(32),
            rate_limit_initial_backoff: Duration::from_secs(5),
            rate_limit_max_backoff: Duration::from_secs(60),
            rate_limit_max_wait: Duration::from_secs(180),
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
            rate_limit_initial_backoff: Duration::ZERO,
            rate_limit_max_backoff: Duration::ZERO,
            rate_limit_max_wait: Duration::ZERO,
        }
    }

    /// The delay before retry `attempt` (0 for the first retry) of a server
    /// error or timeout.
    pub fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        jittered(self.initial_backoff, attempt, self.max_backoff, retry_after)
    }

    /// The delay before retry `attempt` (0 for the first retry) of a rate limit.
    pub fn rate_limit_backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        jittered(
            self.rate_limit_initial_backoff,
            attempt,
            self.rate_limit_max_backoff,
            retry_after,
        )
    }
}

/// `initial * 2^attempt`, capped at `max`, jittered into its upper half, and at
/// least `retry_after` (still capped at `max`).
fn jittered(
    initial: Duration,
    attempt: u32,
    max: Duration,
    retry_after: Option<Duration>,
) -> Duration {
    let exp = initial
        .saturating_mul(2_u32.saturating_pow(attempt))
        .min(max);
    let half = exp / 2;
    let jitter = Duration::from_nanos(fastrand::u64(
        0..=u64::try_from((exp - half).as_nanos()).unwrap_or(u64::MAX),
    ));
    let delay = half + jitter;
    retry_after.map_or(delay, |wait| delay.max(wait).min(max))
}

/// Tracks one request's retries against a [`RetryPolicy`].
#[derive(Debug)]
pub(crate) struct Retrier {
    policy: RetryPolicy,
    attempts: u32,
    rate_limit_attempts: u32,
    rate_limit_waited: Duration,
}

impl Retrier {
    pub(crate) fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            attempts: 0,
            rate_limit_attempts: 0,
            rate_limit_waited: Duration::ZERO,
        }
    }

    /// How long to wait before retrying a retryable failure, or `None` if its
    /// retries are used up. `rate_limited` picks the rate-limit budget.
    pub(crate) fn next_delay(
        &mut self,
        rate_limited: bool,
        retry_after: Option<Duration>,
    ) -> Option<Duration> {
        if rate_limited {
            let left = self
                .policy
                .rate_limit_max_wait
                .saturating_sub(self.rate_limit_waited);
            if left.is_zero() {
                return None;
            }
            let delay = self
                .policy
                .rate_limit_backoff(self.rate_limit_attempts, retry_after)
                .min(left);
            self.rate_limit_attempts += 1;
            self.rate_limit_waited += delay;
            Some(delay)
        } else {
            if self.attempts >= self.policy.max_retries {
                return None;
            }
            let delay = self.policy.backoff(self.attempts, retry_after);
            self.attempts += 1;
            Some(delay)
        }
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
            ..RetryPolicy::default()
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
            ..RetryPolicy::default()
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
        let mut retrier = Retrier::new(RetryPolicy::none());
        assert_eq!(retrier.next_delay(false, None), None);
        assert_eq!(retrier.next_delay(true, None), None);
    }

    #[test]
    fn server_errors_use_max_retries() {
        let mut retrier = Retrier::new(RetryPolicy {
            max_retries: 2,
            ..RetryPolicy::default()
        });
        assert!(retrier.next_delay(false, None).is_some());
        assert!(retrier.next_delay(false, None).is_some());
        assert_eq!(retrier.next_delay(false, None), None);
    }

    #[test]
    fn rate_limits_back_off_past_a_minute_window() {
        let policy = RetryPolicy::default();
        let mut retrier = Retrier::new(policy);
        let mut total = Duration::ZERO;
        let mut delays = Vec::new();
        while let Some(delay) = retrier.next_delay(true, None) {
            assert!(delay <= policy.rate_limit_max_backoff);
            total += delay;
            delays.push(delay);
        }
        // The default stops after three minutes of waiting, well past the
        // per-minute window, and not before.
        assert_eq!(total, policy.rate_limit_max_wait);
        assert!(delays.len() > policy.max_retries as usize, "{delays:?}");
        // Rate-limit retries do not use up server-error retries.
        assert!(retrier.next_delay(false, None).is_some());
    }

    #[test]
    fn rate_limits_back_off_for_seconds_not_milliseconds() {
        let policy = RetryPolicy::default();
        for _ in 0..100 {
            let first = policy.rate_limit_backoff(0, None);
            assert!((Duration::from_millis(2500)..=Duration::from_secs(5)).contains(&first));
            let third = policy.rate_limit_backoff(2, None);
            assert!((Duration::from_secs(10)..=Duration::from_secs(20)).contains(&third));
            // Server errors keep their short backoff.
            assert!(policy.backoff(0, None) <= Duration::from_millis(500));
        }
    }

    #[test]
    fn rate_limits_honour_retry_after_up_to_their_cap() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.rate_limit_backoff(0, Some(Duration::from_secs(45))),
            Duration::from_secs(45)
        );
        assert_eq!(
            policy.rate_limit_backoff(0, Some(Duration::from_secs(600))),
            Duration::from_secs(60)
        );
    }
}
