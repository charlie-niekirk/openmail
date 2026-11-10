//! Gmail quota units: what each call costs, and a limiter that keeps a client
//! under the per-user rate limit.
//!
//! Gmail charges every call a documented number of quota units and limits each
//! user to 15,000 units a minute (a moving average of about 250 a second).
//! [`GmailClient`](super::GmailClient) charges each request, and each item of a
//! batch, to a token bucket shared by all its clones before sending it.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::Method;
use tokio::time::Instant;

/// How fast a client may spend Gmail quota units.
///
/// The limiter is a token bucket: up to `burst` units can be spent at once, and
/// the bucket refills at `units_per_second`. Over any minute a client spends at
/// most `burst + 60 * units_per_second` units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaLimit {
    /// The sustained rate.
    pub units_per_second: u32,
    /// The most units spent at once. A batch of 50 `threads.get` costs 500.
    pub burst: u32,
}

impl Default for QuotaLimit {
    /// 200 units a second with bursts of 1,000: at most 13,000 units in any
    /// minute, under Gmail's 15,000.
    fn default() -> Self {
        Self {
            units_per_second: 200,
            burst: 1000,
        }
    }
}

/// A token bucket of quota units, shared by a client's clones.
///
/// Callers reserve units in order: a reservation the bucket cannot cover yet
/// takes them anyway (going into debt) and waits until the debt is repaid, so
/// later callers queue behind it and a call costing more than `burst` still
/// goes through.
#[derive(Debug)]
pub(crate) struct QuotaLimiter {
    limit: QuotaLimit,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// Units available; negative while callers wait for reserved units.
    units: f64,
    /// When `units` was last brought up to date. `None` until first use, so
    /// the clock is read inside the runtime.
    updated: Option<Instant>,
}

impl QuotaLimiter {
    pub(crate) fn new(limit: QuotaLimit) -> Self {
        Self {
            limit,
            state: Mutex::new(Bucket {
                units: f64::from(limit.burst),
                updated: None,
            }),
        }
    }

    pub(crate) fn limit(&self) -> QuotaLimit {
        self.limit
    }

    /// Reserves `cost` units and returns how long to wait before spending them.
    pub(crate) fn reserve(&self, cost: u32) -> Duration {
        let rate = f64::from(self.limit.units_per_second.max(1));
        let burst = f64::from(self.limit.burst);
        let now = Instant::now();
        let mut bucket = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(updated) = bucket.updated {
            let refill = now.saturating_duration_since(updated).as_secs_f64() * rate;
            bucket.units = (bucket.units + refill).min(burst);
        }
        bucket.updated = Some(now);
        bucket.units -= f64::from(cost);
        if bucket.units >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-bucket.units / rate)
        }
    }

    /// Waits until `cost` units may be spent.
    pub(crate) async fn acquire(&self, cost: u32) {
        let wait = self.reserve(cost);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// The quota units Gmail charges for `method` on `users/me/<segments>`, from
/// Gmail's usage limits documentation. Unknown calls are charged 10 units.
pub(crate) fn cost(method: &Method, segments: &[&str]) -> u32 {
    let get = *method == Method::GET;
    let delete = *method == Method::DELETE;
    match segments {
        ["profile"] => 1,
        ["history"] => 2,
        ["labels"] if get => 1,
        ["labels", _] if get => 1,
        ["labels", ..] => 5,
        ["threads"] => 10,
        ["threads", _] if delete => 20,
        ["threads", ..] => 10,
        ["messages", "batchModify" | "batchDelete"] => 50,
        ["messages", "send"] => 100,
        ["messages", "import" | "insert"] => 25,
        ["messages"] => 5,
        ["messages", _] if delete => 10,
        ["messages", ..] => 5,
        ["drafts", _, "send"] | ["drafts", "send"] => 100,
        ["drafts"] if get => 5,
        ["drafts"] => 10,
        ["drafts", _] if get => 5,
        ["drafts", _] if delete => 10,
        ["drafts", _] => 15,
        ["settings", ..] if get => 1,
        ["settings", ..] => 5,
        ["watch"] => 100,
        ["stop"] => 50,
        _ => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_cost_their_documented_units() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(cost(&get, &["profile"]), 1);
        assert_eq!(cost(&get, &["labels"]), 1);
        assert_eq!(cost(&get, &["labels", "INBOX"]), 1);
        assert_eq!(cost(&post, &["labels"]), 5);
        assert_eq!(cost(&Method::PATCH, &["labels", "Label_1"]), 5);
        assert_eq!(cost(&get, &["threads"]), 10);
        assert_eq!(cost(&get, &["threads", "t"]), 10);
        assert_eq!(cost(&post, &["threads", "t", "modify"]), 10);
        assert_eq!(cost(&Method::DELETE, &["threads", "t"]), 20);
        assert_eq!(cost(&get, &["messages"]), 5);
        assert_eq!(cost(&get, &["messages", "m"]), 5);
        assert_eq!(cost(&get, &["messages", "m", "attachments", "a"]), 5);
        assert_eq!(cost(&post, &["messages", "m", "modify"]), 5);
        assert_eq!(cost(&post, &["messages", "m", "trash"]), 5);
        assert_eq!(cost(&Method::DELETE, &["messages", "m"]), 10);
        assert_eq!(cost(&post, &["messages", "batchModify"]), 50);
        assert_eq!(cost(&post, &["messages", "send"]), 100);
        assert_eq!(cost(&get, &["history"]), 2);
        assert_eq!(cost(&post, &["drafts", "send"]), 100);
        assert_eq!(cost(&get, &["somethingNew"]), 10);
    }

    #[tokio::test(start_paused = true)]
    async fn spends_the_burst_at_once_then_waits_for_refill() {
        let limiter = QuotaLimiter::new(QuotaLimit {
            units_per_second: 100,
            burst: 500,
        });
        assert_eq!(limiter.reserve(500), Duration::ZERO);
        // Empty: 200 more units take 2 s to refill.
        assert_eq!(limiter.reserve(200), Duration::from_secs(2));
        // Reservations queue: the next 100 wait behind the 200.
        assert_eq!(limiter.reserve(100), Duration::from_secs(3));

        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(limiter.reserve(100), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn refill_is_capped_at_the_burst() {
        let limiter = QuotaLimiter::new(QuotaLimit {
            units_per_second: 100,
            burst: 300,
        });
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(limiter.reserve(300), Duration::ZERO);
        assert_eq!(limiter.reserve(100), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn calls_larger_than_the_burst_go_through_after_a_wait() {
        let limiter = QuotaLimiter::new(QuotaLimit {
            units_per_second: 10,
            burst: 20,
        });
        assert_eq!(limiter.reserve(50), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_sleeps_until_the_units_are_available() {
        let limiter = QuotaLimiter::new(QuotaLimit {
            units_per_second: 50,
            burst: 100,
        });
        let start = Instant::now();
        limiter.acquire(100).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        limiter.acquire(100).await;
        limiter.acquire(50).await;
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn stays_under_gmails_per_minute_limit_by_default() {
        let limiter = QuotaLimiter::new(QuotaLimit::default());
        let start = Instant::now();
        let mut spent = 0;
        // Batches of 50 threads.get, back to back, for one minute.
        while start.elapsed() < Duration::from_secs(60) {
            limiter.acquire(500).await;
            if start.elapsed() < Duration::from_secs(60) {
                spent += 500;
            }
        }
        assert!(spent <= 15_000, "{spent}");
        assert!(spent >= 12_000, "{spent}");
    }
}
