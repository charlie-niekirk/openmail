//! Gmail quota units: what each call costs, and a limiter that keeps a client
//! under the per-user rate limit.
//!
//! Gmail charges every call a documented number of quota units and limits each
//! user to a number of units a minute: 6,000 by default for new Cloud projects
//! (it used to be 15,000; a project owner can raise it). The limit is enforced
//! over short windows too, so a client has to spread its spending evenly.
//! [`GmailClient`](super::GmailClient) charges each request, and each item of a
//! batch, to a token bucket shared by all its clones before sending it.
//!
//! By default the bucket's rate adapts ([`AdaptiveRate`]): it starts at 80% of
//! the per-minute budget, and when Gmail rate-limits a request anyway it stops
//! sending for a while (Gmail keeps rejecting for up to a minute), remembers
//! the rate that tripped it, resumes below it and probes back up slowly.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::Method;
use tokio::time::Instant;

/// Gmail's default "Units per minute per user" quota for a new Cloud project.
pub const DEFAULT_QUOTA_PER_MINUTE: u32 = 6_000;

/// The environment variable that overrides [`DEFAULT_QUOTA_PER_MINUTE`], for
/// projects whose quota has been raised.
pub const QUOTA_PER_MINUTE_ENV: &str = "OPENMAIL_GMAIL_QUOTA_PER_MINUTE";

/// A fixed rate at which a client may spend Gmail quota units.
///
/// The limiter is a token bucket: up to `burst` units can be spent at once, and
/// the bucket refills at `units_per_second`. Over any minute a client spends at
/// most `burst + 60 * units_per_second` units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaLimit {
    /// The sustained rate.
    pub units_per_second: u32,
    /// The most units spent at once. A batch of 5 `threads.get` costs 50.
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
    /// 80 units a second with bursts of 80: at most 4,880 units in any
    /// minute, 80% of Gmail's default 6,000.
    fn default() -> Self {
        Self::per_second(80)
    }
}

/// A quota rate that adapts to Gmail's rate limiting.
///
/// It starts at the budget, `utilization_percent` of `units_per_minute`.
/// When Gmail rate-limits a request sent at rate `R`, nothing more is sent
/// for `lockout` (doubled, up to `max_lockout`, if the first requests after a
/// lockout are limited again), and sending resumes at 70% of `R`, the
/// remembered ceiling. From there the rate rises by `probe_step` after every
/// `probe_every` without a rate limit, up to the budget. Bursts are half a
/// second's worth at the current rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveRate {
    /// The project's "Units per minute per user" quota.
    pub units_per_minute: u32,
    /// The share of it to use, leaving headroom for other clients and for
    /// Gmail's short-window enforcement.
    pub utilization_percent: u32,
    /// The lowest rate, in units per second.
    pub min: u32,
    /// How long to stop sending after a rate limit, at first.
    pub lockout: Duration,
    /// The longest lockout.
    pub max_lockout: Duration,
    /// Units per second added after each clean `probe_every`.
    pub probe_step: u32,
    /// How long without a rate limit before each step up.
    pub probe_every: Duration,
}

impl AdaptiveRate {
    /// The defaults, for a project allowing `units_per_minute` per user.
    pub fn per_minute(units_per_minute: u32) -> Self {
        Self {
            units_per_minute,
            ..Self::default()
        }
    }

    /// The defaults, with the per-minute quota from [`QUOTA_PER_MINUTE_ENV`]
    /// if it is set to a positive number (otherwise
    /// [`DEFAULT_QUOTA_PER_MINUTE`]).
    pub fn from_env() -> Self {
        let configured = std::env::var(QUOTA_PER_MINUTE_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|&units| units > 0);
        Self::per_minute(configured.unwrap_or(DEFAULT_QUOTA_PER_MINUTE))
    }

    /// The highest rate it uses, in units per second.
    pub fn budget(&self) -> u32 {
        let per_minute = u64::from(self.units_per_minute) * u64::from(self.utilization_percent);
        u32::try_from(per_minute / 100 / 60)
            .unwrap_or(u32::MAX)
            .max(self.min.max(1))
    }
}

impl Default for AdaptiveRate {
    /// 80% of 6,000 units a minute (80 units/s), never below 10/s; a 15 s
    /// lockout growing to 2 min; probing up by 5 units/s every 30 s.
    fn default() -> Self {
        Self {
            units_per_minute: DEFAULT_QUOTA_PER_MINUTE,
            utilization_percent: 80,
            min: 10,
            lockout: Duration::from_secs(15),
            max_lockout: Duration::from_secs(120),
            probe_step: 5,
            probe_every: Duration::from_secs(30),
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
    Adaptive { config: AdaptiveRate, budget: u32 },
}

#[derive(Debug)]
struct Bucket {
    /// Units available; negative while callers wait for reserved units.
    units: f64,
    /// Units per second.
    rate: u32,
    /// When `units` was last brought up to date: the bucket fills from then.
    /// `None` until first use, so the clock is read inside the runtime. During
    /// a lockout it is the lockout's end, so nothing accrues before it.
    updated: Option<Instant>,
    /// When the next probe step may happen.
    next_probe: Option<Instant>,
    /// The last rate limit: when it was recorded, and when sending resumed
    /// after it. Rate limits on requests sent before it are ignored.
    tripped: Option<(Instant, Instant)>,
    /// The current lockout length.
    lockout: Duration,
    /// The rate Gmail last rate-limited at.
    ceiling: Option<u32>,
}

impl QuotaLimiter {
    pub(crate) fn new(pacing: QuotaPacing) -> Option<Self> {
        let (pacing, rate, lockout) = match pacing {
            QuotaPacing::Off => return None,
            QuotaPacing::Fixed(limit) => (
                Pacing::Fixed { burst: limit.burst },
                limit.units_per_second.max(1),
                Duration::ZERO,
            ),
            QuotaPacing::Adaptive(config) => {
                let config = AdaptiveRate {
                    min: config.min.max(1),
                    max_lockout: config.max_lockout.max(config.lockout),
                    ..config
                };
                let budget = config.budget();
                (Pacing::Adaptive { config, budget }, budget, config.lockout)
            }
        };
        let limiter = Self {
            pacing,
            state: Mutex::new(Bucket {
                units: 0.0,
                rate,
                updated: None,
                next_probe: None,
                tripped: None,
                lockout,
                ceiling: None,
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
            Pacing::Adaptive { .. } => f64::from(rate) / 2.0,
        }
    }

    /// The current rate in units per second.
    pub(crate) fn rate(&self) -> u32 {
        let mut bucket = self.bucket();
        self.advance(&mut bucket, Instant::now());
        bucket.rate
    }

    /// Brings the bucket up to `now`: refills it, stepping an adaptive rate up
    /// at each probe time on the way.
    fn advance(&self, bucket: &mut Bucket, now: Instant) {
        let Some(mut updated) = bucket.updated else {
            bucket.updated = Some(now);
            return;
        };
        if let Pacing::Adaptive { config, budget } = self.pacing
            && !config.probe_every.is_zero()
        {
            let mut next = *bucket
                .next_probe
                .get_or_insert(updated + config.probe_every);
            while bucket.rate < budget && next <= now {
                self.refill(bucket, updated, next);
                updated = updated.max(next);
                let old = bucket.rate;
                bucket.rate = (bucket.rate + config.probe_step).min(budget);
                tracing::debug!(
                    from = old,
                    to = bucket.rate,
                    "no Gmail rate limits; probing a higher quota rate"
                );
                next += config.probe_every;
                bucket.next_probe = Some(next);
            }
        }
        if now > updated {
            self.refill(bucket, updated, now);
            bucket.updated = Some(now);
        }
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
        // During a lockout `updated` is its end: the debt is repaid from then.
        let start = bucket.updated.map_or(Duration::ZERO, |at| at - now.min(at));
        if bucket.units >= 0.0 {
            start
        } else {
            start + Duration::from_secs_f64(-bucket.units / f64::from(bucket.rate))
        }
    }

    /// Waits until `cost` units may be spent.
    pub(crate) async fn acquire(&self, cost: u32) {
        let wait = self.reserve(cost);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Records that Gmail rate-limited a request sent at `sent`, and returns
    /// the new rate if an adaptive rate changed.
    ///
    /// Requests sent before the last rate limit are ignored: the old rate
    /// caused them. Otherwise sending stops for the lockout. If the request
    /// was among the first after the last lockout, Gmail is still locking the
    /// user out, so the lockout doubles and the rate stays; if not, the
    /// lockout starts over and the rate drops to 70% of the rate that was
    /// limited, which is remembered as the ceiling.
    pub(crate) fn rate_limited(&self, sent: Instant) -> Option<u32> {
        let Pacing::Adaptive { config, .. } = self.pacing else {
            return None;
        };
        let now = Instant::now();
        let mut bucket = self.bucket();
        let previous = bucket.tripped;
        if previous.is_some_and(|(at, _)| sent < at) {
            return None;
        }
        self.advance(&mut bucket, now);
        let old = bucket.rate;
        let still_locked_out = previous.is_some_and(|(_, resumed)| sent < resumed + config.lockout);
        if still_locked_out {
            bucket.lockout = (bucket.lockout * 2).min(config.max_lockout);
        } else {
            bucket.lockout = config.lockout;
            bucket.ceiling = Some(old);
            bucket.rate = (old * 7 / 10).max(config.min);
        }
        let resume = now + bucket.lockout;
        bucket.units = bucket.units.min(0.0);
        bucket.updated = Some(resume);
        bucket.next_probe = Some(resume + config.probe_every);
        bucket.tripped = Some((now, resume));
        tracing::debug!(
            ceiling = bucket.ceiling,
            from = old,
            to = bucket.rate,
            lockout_s = bucket.lockout.as_secs(),
            "Gmail rate limit; pausing, then resuming below the limited rate"
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

    async fn advance(secs: f64) {
        tokio::time::advance(Duration::from_secs_f64(secs)).await;
    }

    fn secs(d: Duration) -> f64 {
        d.as_secs_f64()
    }

    #[test]
    fn the_default_budget_is_80_percent_of_6000_a_minute() {
        let config = AdaptiveRate::default();
        assert_eq!(config.units_per_minute, 6_000);
        assert_eq!(config.budget(), 80);
        assert_eq!(AdaptiveRate::per_minute(15_000).budget(), 200);
        // Never below the minimum.
        assert_eq!(AdaptiveRate::per_minute(60).budget(), 10);
        assert!(QuotaLimiter::new(QuotaPacing::Off).is_none());
        assert_eq!(
            QuotaPacing::default(),
            QuotaPacing::Adaptive(AdaptiveRate::default())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn starts_at_the_budget_with_half_second_bursts() {
        let limiter = adaptive(AdaptiveRate::default());
        advance(300.0).await;
        assert_eq!(limiter.rate(), 80);
        // The bucket holds 40 units, so a batch of 5 threads.get (50 units)
        // waits 125 ms, and the next one waits its full 625 ms after that.
        assert_eq!(limiter.reserve(40), Duration::ZERO);
        assert_eq!(limiter.reserve(50), Duration::from_millis(625));
        assert_eq!(limiter.reserve(50), Duration::from_millis(1250));
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_stops_sending_for_the_lockout_then_resumes_lower() {
        let limiter = adaptive(AdaptiveRate::default());
        limiter.reserve(40);
        assert_eq!(limiter.rate_limited(Instant::now()), Some(56));
        // Nothing until the 15 s lockout ends, and no burst after it: the
        // first 28 units take half a second at 56/s.
        assert_eq!(secs(limiter.reserve(28)), 15.5);
        advance(15.5).await;
        assert_eq!(limiter.reserve(0), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_sent_before_a_rate_limit_are_ignored() {
        let limiter = adaptive(AdaptiveRate::default());
        let in_flight = Instant::now();
        advance(1.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), Some(56));
        assert_eq!(limiter.rate_limited(in_flight), None);
        assert_eq!(limiter.rate(), 56);
    }

    #[tokio::test(start_paused = true)]
    async fn a_limit_right_after_a_lockout_doubles_it_and_keeps_the_rate() {
        let limiter = adaptive(AdaptiveRate::default());
        assert_eq!(limiter.rate_limited(Instant::now()), Some(56));
        // The first request after the lockout is limited too.
        advance(15.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), None);
        assert_eq!(limiter.rate(), 56);
        assert_eq!(secs(limiter.reserve(0)), 30.0);
        advance(30.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), None);
        assert_eq!(secs(limiter.reserve(0)), 60.0);
        advance(60.0).await;
        assert_eq!(limiter.rate_limited(Instant::now()), None);
        // Capped at two minutes.
        assert_eq!(secs(limiter.reserve(0)), 120.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_limit_restarts_the_lockout_and_lowers_the_ceiling() {
        let limiter = adaptive(AdaptiveRate::default());
        limiter.rate_limited(Instant::now());
        advance(15.0).await;
        limiter.rate_limited(Instant::now());
        // Clean well past the lockout (which ended at 45 s), then limited
        // again at 56: the 30 s lockout is forgotten, rate to 70% of 56.
        advance(50.0).await;
        assert_eq!(limiter.rate(), 56);
        assert_eq!(limiter.rate_limited(Instant::now()), Some(39));
        assert_eq!(secs(limiter.reserve(0)), 15.0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_probes_up_slowly_to_the_budget() {
        let limiter = adaptive(AdaptiveRate::default());
        limiter.rate_limited(Instant::now());
        // Lockout ends at 15 s; the first probe is 30 s later.
        advance(44.0).await;
        assert_eq!(limiter.rate(), 56);
        advance(1.0).await;
        assert_eq!(limiter.rate(), 61);
        advance(30.0).await;
        assert_eq!(limiter.rate(), 66);
        advance(3_600.0).await;
        assert_eq!(limiter.rate(), 80);
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_never_drops_below_its_minimum() {
        let limiter = adaptive(AdaptiveRate {
            lockout: Duration::from_secs(1),
            ..AdaptiveRate::default()
        });
        let mut rates = Vec::new();
        for _ in 0..8 {
            advance(10.0).await;
            limiter.rate_limited(Instant::now());
            rates.push(limiter.rate());
        }
        assert_eq!(rates, [56, 39, 27, 18, 12, 10, 10, 10]);
    }

    #[tokio::test(start_paused = true)]
    async fn refill_uses_the_rate_in_force_at_each_moment() {
        let limiter = adaptive(AdaptiveRate {
            probe_step: 40,
            probe_every: Duration::from_secs(10),
            ..AdaptiveRate::default()
        });
        limiter.rate_limited(Instant::now());
        assert_eq!(limiter.rate(), 56);
        // Resumes at 15 s, probes to 80 at 25 s: the bucket is full again
        // (half a second at 80).
        advance(30.0).await;
        assert_eq!(limiter.rate(), 80);
        assert_eq!(limiter.reserve(40), Duration::ZERO);
        assert_eq!(limiter.reserve(40), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn the_adaptive_default_spends_under_the_per_minute_quota() {
        let limiter = adaptive(AdaptiveRate::default());
        let start = Instant::now();
        let mut spent = 0;
        while start.elapsed() < Duration::from_secs(60) {
            limiter.acquire(50).await;
            if start.elapsed() < Duration::from_secs(60) {
                spent += 50;
            }
        }
        assert!((4_500..=4_900).contains(&spent), "{spent}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_fixed_rate_ignores_rate_limits() {
        let limiter = fixed(QuotaLimit::per_second(100));
        assert_eq!(limiter.rate_limited(Instant::now()), None);
        assert_eq!(limiter.rate(), 100);
        assert_eq!(limiter.reserve(100), Duration::ZERO);
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
    async fn a_fixed_default_stays_under_the_default_per_minute_quota() {
        let limiter = fixed(QuotaLimit::default());
        let start = Instant::now();
        let mut spent = 0;
        // Batches of 5 threads.get, back to back, for one minute.
        while start.elapsed() < Duration::from_secs(60) {
            limiter.acquire(50).await;
            if start.elapsed() < Duration::from_secs(60) {
                spent += 50;
            }
        }
        assert!(spent <= DEFAULT_QUOTA_PER_MINUTE, "{spent}");
        assert!(spent >= 4_500, "{spent}");
    }
}
