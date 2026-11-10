//! Gmail quota units: what each call costs, and a limiter that keeps a client
//! under the per-user rate limit.
//!
//! Gmail charges every call a documented number of quota units and limits each
//! user to 15,000 units a minute (a moving average of about 250 a second).
//! [`GmailClient`](super::GmailClient) charges each request, and each item of a
//! batch, to a token bucket shared by all its clones before sending it.
//!
//! By default the bucket's rate adapts: Gmail's effective limit is lower than
//! the documented one (failed calls seem to count too), so the rate is halved
//! whenever Gmail answers with a rate limit and creeps back up while it does
//! not, like TCP congestion control.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::Method;
use tokio::time::Instant;

/// A fixed rate at which a client may spend Gmail quota units.
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

impl QuotaLimit {
    /// `units_per_second`, with bursts of one second's worth.
    pub const fn per_second(units_per_second: u32) -> Self {
        Self {
            units_per_second,
            burst: units_per_second,
        }
    }
}

impl Default for QuotaLimit {
    /// 200 units a second with bursts of 200: at most 12,200 units in any
    /// minute, under Gmail's 15,000. A large burst after an idle spell is
    /// enough on its own to trip Gmail's moving average.
    fn default() -> Self {
        Self::per_second(200)
    }
}

/// A quota rate that adapts to Gmail's rate limiting: halved (with the bucket
/// emptied) when Gmail rate-limits a request, and raised by `step` after every
/// `step_every` without one. Bursts are one second's worth at the current
/// rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveRate {
    /// Units per second to start at.
    pub initial: u32,
    /// The lowest rate halving goes to.
    pub min: u32,
    /// The highest rate recovery goes to.
    pub max: u32,
    /// Units per second added after each clean `step_every`.
    pub step: u32,
    /// How long without a rate limit before each step up.
    pub step_every: Duration,
}

impl Default for AdaptiveRate {
    /// Start at 150 units/s, halve down to 25, and add 10 every 10 s up to 240.
    fn default() -> Self {
        Self {
            initial: 150,
            min: 25,
            max: 240,
            step: 10,
            step_every: Duration::from_secs(10),
        }
    }
}

/// How a client paces the quota units it spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPacing {
    /// No client-side limit.
    Off,
    /// A fixed rate.
    Fixed(QuotaLimit),
    /// A rate adapted to Gmail's rate limiting.
    Adaptive(AdaptiveRate),
}

impl Default for QuotaPacing {
    fn default() -> Self {
        Self::Adaptive(AdaptiveRate::default())
    }
}

/// A token bucket of quota units, shared by a client's clones.
///
/// Callers reserve units in order: a reservation the bucket cannot cover yet
/// takes them anyway (going into debt) and waits until the debt is repaid, so
/// later callers queue behind it and a call costing more than the burst still
/// goes through.
#[derive(Debug)]
pub(crate) struct QuotaLimiter {
    pacing: Pacing,
    state: Mutex<Bucket>,
}

#[derive(Debug, Clone, Copy)]
enum Pacing {
    Fixed { burst: u32 },
    Adaptive(AdaptiveRate),
}

#[derive(Debug)]
struct Bucket {
    /// Units available; negative while callers wait for reserved units.
    units: f64,
    /// Units per second.
    rate: u32,
    /// When `units` was last brought up to date. `None` until first use, so
    /// the clock is read inside the runtime.
    updated: Option<Instant>,
    /// When the rate last changed (or the bucket was first used): the next
    /// step up is `step_every` after it.
    changed: Option<Instant>,
    /// When the rate was last halved. Rate limits on requests sent before
    /// then were caused by the old rate, so they do not halve it again.
    decreased: Option<Instant>,
}

impl QuotaLimiter {
    pub(crate) fn new(pacing: QuotaPacing) -> Option<Self> {
        let (pacing, rate) = match pacing {
            QuotaPacing::Off => return None,
            QuotaPacing::Fixed(limit) => (
                Pacing::Fixed { burst: limit.burst },
                limit.units_per_second.max(1),
            ),
            QuotaPacing::Adaptive(config) => {
                let min = config.min.max(1);
                let config = AdaptiveRate {
                    min,
                    max: config.max.max(min),
                    ..config
                };
                (
                    Pacing::Adaptive(config),
                    config.initial.clamp(config.min, config.max),
                )
            }
        };
        let limiter = Self {
            pacing,
            state: Mutex::new(Bucket {
                units: 0.0,
                rate,
                updated: None,
                changed: None,
                decreased: None,
            }),
        };
        limiter.bucket().units = limiter.burst(rate);
        Some(limiter)
    }

    fn bucket(&self) -> std::sync::MutexGuard<'_, Bucket> {
        // The state is a few numbers, valid even if a holder panicked.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn burst(&self, rate: u32) -> f64 {
        match self.pacing {
            Pacing::Fixed { burst } => f64::from(burst),
            Pacing::Adaptive(_) => f64::from(rate),
        }
    }

    /// The current rate in units per second.
    pub(crate) fn rate(&self) -> u32 {
        let mut bucket = self.bucket();
        self.advance(&mut bucket, Instant::now());
        bucket.rate
    }

    /// Brings the bucket up to `now`: refills it, stepping the adaptive rate
    /// up at each `step_every` boundary on the way.
    fn advance(&self, bucket: &mut Bucket, now: Instant) {
        let (Some(mut updated), Some(changed)) = (bucket.updated, bucket.changed) else {
            bucket.updated = Some(now);
            bucket.changed = Some(now);
            return;
        };
        if let Pacing::Adaptive(config) = self.pacing
            && !config.step_every.is_zero()
        {
            let mut next = changed + config.step_every;
            while bucket.rate < config.max && next <= now {
                self.refill(bucket, updated, next);
                updated = next;
                let old = bucket.rate;
                bucket.rate = (bucket.rate + config.step).min(config.max);
                bucket.changed = Some(next);
                tracing::debug!(
                    from = old,
                    to = bucket.rate,
                    "no Gmail rate limits; raising the quota rate"
                );
                next += config.step_every;
            }
        }
        self.refill(bucket, updated, now);
        bucket.updated = Some(now);
    }

    fn refill(&self, bucket: &mut Bucket, from: Instant, to: Instant) {
        let elapsed = to.saturating_duration_since(from).as_secs_f64();
        let burst = self.burst(bucket.rate);
        bucket.units = (bucket.units + elapsed * f64::from(bucket.rate)).min(burst);
    }

    /// Reserves `cost` units and returns how long to wait before spending them.
    pub(crate) fn reserve(&self, cost: u32) -> Duration {
        let now = Instant::now();
        let mut bucket = self.bucket();
        self.advance(&mut bucket, now);
        bucket.units -= f64::from(cost);
        if bucket.units >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-bucket.units / f64::from(bucket.rate))
        }
    }

    /// Waits until `cost` units may be spent.
    pub(crate) async fn acquire(&self, cost: u32) {
        let wait = self.reserve(cost);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Records that Gmail rate-limited a request sent at `sent`. An adaptive
    /// rate is halved (down to its minimum) and the bucket emptied, so nothing
    /// is sent in a burst straight after; a rate limit on a request sent
    /// before the last halving is ignored. Returns the new rate if it changed.
    pub(crate) fn rate_limited(&self, sent: Instant) -> Option<u32> {
        let Pacing::Adaptive(config) = self.pacing else {
            return None;
        };
        let now = Instant::now();
        let mut bucket = self.bucket();
        if bucket.decreased.is_some_and(|at| sent < at) {
            return None;
        }
        self.advance(&mut bucket, now);
        let old = bucket.rate;
        bucket.rate = (bucket.rate / 2).max(config.min);
        bucket.units = bucket.units.min(0.0);
        bucket.changed = Some(now);
        bucket.decreased = Some(now);
        tracing::debug!(
            from = old,
            to = bucket.rate,
            "Gmail rate limit; halving the quota rate"
        );
        (bucket.rate != old).then_some(bucket.rate)
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

    fn fixed(limit: QuotaLimit) -> QuotaLimiter {
        QuotaLimiter::new(QuotaPacing::Fixed(limit)).unwrap()
    }

    fn adaptive(config: AdaptiveRate) -> QuotaLimiter {
        QuotaLimiter::new(QuotaPacing::Adaptive(config)).unwrap()
    }

    fn config() -> AdaptiveRate {
        AdaptiveRate {
            initial: 160,
            min: 25,
            max: 240,
            step: 10,
            step_every: Duration::from_secs(10),
        }
    }

    async fn advance(secs: f64) {
        tokio::time::advance(Duration::from_secs_f64(secs)).await;
    }

    #[test]
    fn pacing_can_be_off() {
        assert!(QuotaLimiter::new(QuotaPacing::Off).is_none());
        assert_eq!(
            QuotaPacing::default(),
            QuotaPacing::Adaptive(AdaptiveRate::default())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_adaptive_burst_is_one_second_at_the_current_rate() {
        let limiter = adaptive(config());
        // Long idle: still only one second's worth.
        advance(300.0).await;
        assert_eq!(limiter.rate(), 160);
        assert_eq!(limiter.reserve(160), Duration::ZERO);
        assert_eq!(limiter.reserve(80), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_halves_the_rate_and_empties_the_bucket() {
        let limiter = adaptive(config());
        assert_eq!(limiter.rate(), 160);
        assert_eq!(limiter.rate_limited(Instant::now()), Some(80));
        // Nothing left: 80 units take a second at the new rate.
        assert_eq!(limiter.reserve(80), Duration::from_secs(1));
        // Debt is kept, not forgiven: 40 units still owed at 40/s.
        advance(0.5).await;
        assert_eq!(limiter.rate_limited(Instant::now()), Some(40));
        assert_eq!(limiter.reserve(0), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn requests_sent_before_a_halving_do_not_halve_again() {
        let limiter = adaptive(config());
        let early = Instant::now();
        advance(1.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), Some(80));
        // In flight before the halving: ignored.
        assert_eq!(limiter.rate_limited(early), None);
        assert_eq!(limiter.rate(), 80);
        // Sent afterwards: halves.
        advance(1.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), Some(40));
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_never_halves_below_its_minimum() {
        let limiter = adaptive(config());
        let mut rates = Vec::new();
        for _ in 0..5 {
            advance(0.1).await;
            rates.push(limiter.rate_limited(Instant::now()));
        }
        assert_eq!(rates, [Some(80), Some(40), Some(25), None, None]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_recovers_additively_up_to_its_cap() {
        let limiter = adaptive(config());
        limiter.rate_limited(Instant::now());
        assert_eq!(limiter.rate(), 80);
        advance(9.9).await;
        assert_eq!(limiter.rate(), 80);
        advance(0.1).await;
        assert_eq!(limiter.rate(), 90);
        advance(25.0).await;
        assert_eq!(limiter.rate(), 110);
        // A rate limit restarts the clean period.
        advance(4.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), Some(55));
        advance(9.0).await;
        assert_eq!(limiter.rate(), 55);
        advance(1.0).await;
        assert_eq!(limiter.rate(), 65);
        advance(10_000.0).await;
        assert_eq!(limiter.rate(), 240);
    }

    #[tokio::test(start_paused = true)]
    async fn refill_uses_the_rate_in_force_at_each_moment() {
        let limiter = adaptive(AdaptiveRate {
            initial: 100,
            max: 200,
            step: 100,
            ..config()
        });
        // Empty the bucket, then wait 20 s: 10 s at 100/s, then 10 s at 200/s,
        // capped at the new one-second burst of 200.
        assert_eq!(limiter.reserve(100), Duration::ZERO);
        advance(20.0).await;
        assert_eq!(limiter.reserve(200), Duration::ZERO);
        assert_eq!(limiter.reserve(100), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fixed_rate_ignores_rate_limits() {
        let limiter = fixed(QuotaLimit::per_second(100));
        assert_eq!(limiter.rate_limited(Instant::now()), None);
        assert_eq!(limiter.rate(), 100);
        assert_eq!(limiter.reserve(100), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_adaptive_default_stays_well_under_gmails_limit() {
        let limiter = adaptive(AdaptiveRate::default());
        let start = Instant::now();
        let mut spent = 0;
        while start.elapsed() < Duration::from_secs(60) {
            limiter.acquire(100).await;
            if start.elapsed() < Duration::from_secs(60) {
                spent += 100;
            }
        }
        // 150 rising to 200 units/s.
        assert!((10_000..=12_500).contains(&spent), "{spent}");
    }

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
        let limiter = fixed(QuotaLimit {
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
        let limiter = fixed(QuotaLimit {
            units_per_second: 100,
            burst: 300,
        });
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(limiter.reserve(300), Duration::ZERO);
        assert_eq!(limiter.reserve(100), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn calls_larger_than_the_burst_go_through_after_a_wait() {
        let limiter = fixed(QuotaLimit {
            units_per_second: 10,
            burst: 20,
        });
        assert_eq!(limiter.reserve(50), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_sleeps_until_the_units_are_available() {
        let limiter = fixed(QuotaLimit {
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
        let limiter = fixed(QuotaLimit::default());
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
