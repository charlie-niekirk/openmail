//! Gmail quota units: what each call costs, and a limiter that keeps a client
//! under the per-user rate limit.
//!
//! Gmail charges every call a documented number of quota units and limits each
//! user to a number of units a minute: 6,000 by default for new Cloud projects
//! (it used to be 15,000; a project owner can raise it). [`GmailClient`]
//! (super::GmailClient) charges each request, and each item of a batch, to a
//! limiter shared by all its clones before sending it.
//!
//! By default the limiter keeps a ledger of the units spent over the trailing
//! minute and holds that under a budget ([`MinuteBudget`]). The budget starts
//! at 80% of the per-minute quota. When Gmail rate-limits a request anyway, the
//! budget drops to a little under what the ledger had spent in the minute
//! before, and sending waits only until enough units have left the window.
//! After busy minutes without a rate limit the budget creeps back up.

use std::collections::VecDeque;
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

/// A budget of quota units per trailing minute, learned from Gmail's rate
/// limiting.
///
/// The client keeps a ledger of every unit it charges, failed attempts
/// included (Gmail counts those too), and never lets the units charged in any
/// `window` exceed the budget `B`. Within the window it spreads them evenly, at
/// `B / window`, so nothing bursts after an idle spell.
///
/// `B` starts at `utilization_percent` of `units_per_minute`. When Gmail
/// rate-limits a request, `B` becomes `backoff_percent` of what the ledger
/// spent in the window up to then (the learned ceiling, never below `min`),
/// and sending waits only until the window's spend is back under it. Rate
/// limits on requests sent within a window of that one do not lower `B`
/// again: Gmail is still counting what tripped it, so sending waits until all
/// of that has left the window. After each `probe_every` without a rate limit
/// in which the client spent at least 90% of `B`, `B` rises by
/// `probe_percent`, up to where it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinuteBudget {
    /// The project's "Units per minute per user" quota.
    pub units_per_minute: u32,
    /// The share of it to start at and never exceed, leaving headroom for
    /// other clients.
    pub utilization_percent: u32,
    /// The lowest budget, in units per window.
    pub min: u32,
    /// The share of the trailing spend a rate limit sets the budget to.
    pub backoff_percent: u32,
    /// How much a busy, clean `probe_every` raises the budget, in percent.
    pub probe_percent: u32,
    /// How long without a rate limit before each step up.
    pub probe_every: Duration,
    /// The window the quota is counted over: a minute, except in tests.
    pub window: Duration,
}

impl MinuteBudget {
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

    /// The budget it starts at, and the most it allows, per window.
    pub fn max_budget(&self) -> u32 {
        let units = u64::from(self.units_per_minute) * u64::from(self.utilization_percent) / 100;
        u32::try_from(units)
            .unwrap_or(u32::MAX)
            .max(self.min.max(1))
    }
}

impl Default for MinuteBudget {
    /// Starts at 80% of 6,000 units a minute (4,800), never below 600; a rate
    /// limit sets it to 85% of the trailing minute's spend, and each busy,
    /// clean minute raises it by 5%.
    fn default() -> Self {
        Self {
            units_per_minute: DEFAULT_QUOTA_PER_MINUTE,
            utilization_percent: 80,
            min: 600,
            backoff_percent: 85,
            probe_percent: 5,
            probe_every: Duration::from_secs(60),
            window: Duration::from_secs(60),
        }
    }
}

/// A budget a client learned from Gmail's rate limiting, to start the next
/// client for the same account at (see [`GmailClient::learned_budget`]
/// (super::GmailClient::learned_budget)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearnedBudget {
    /// The per-minute quota it was learned under. A budget learned under
    /// another quota is not reused.
    pub units_per_minute: u32,
    /// The budget, in units per minute.
    pub budget: u32,
}

/// How a client paces the quota units it spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPacing {
    /// No client-side limit.
    Off,
    /// A fixed rate.
    Fixed(QuotaLimit),
    /// A trailing-minute budget learned from Gmail's rate limiting.
    Budget(MinuteBudget),
}

impl Default for QuotaPacing {
    fn default() -> Self {
        Self::Budget(MinuteBudget::default())
    }
}

/// What a limiter is doing, for [`ClientStats`](super::ClientStats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuotaSnapshot {
    /// The pace in units per second.
    pub(crate) units_per_second: u32,
    /// The most units allowed in a trailing window.
    pub(crate) budget: u32,
    /// Units charged in the trailing window.
    pub(crate) spent: u32,
}

/// Paces the quota units a client and its clones spend.
#[derive(Debug)]
pub(crate) struct QuotaLimiter {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    pacer: Pacer,
    /// Every charge, at the time it may be sent, oldest first. Charges leave
    /// once they are a window old.
    ledger: VecDeque<Charge>,
    window: Duration,
    next_id: u64,
}

#[derive(Debug, Clone, Copy)]
struct Charge {
    id: u64,
    at: Instant,
    units: u32,
}

#[derive(Debug)]
enum Pacer {
    Fixed(Bucket),
    Budget(Budget),
}

/// A token bucket, for [`QuotaPacing::Fixed`].
#[derive(Debug)]
struct Bucket {
    /// Units per second.
    rate: u32,
    burst: u32,
    /// Units available; negative while callers wait for reserved units.
    units: f64,
    /// When `units` was last brought up to date. `None` until first use, so
    /// the clock is read inside the runtime.
    updated: Option<Instant>,
}

/// The learned budget, for [`QuotaPacing::Budget`].
#[derive(Debug)]
struct Budget {
    config: MinuteBudget,
    /// Units allowed per window.
    budget: u32,
    /// The earliest the next charge may go, to spread charges evenly.
    next_free: Option<Instant>,
    /// When Gmail last rate-limited a request and the budget was set from it.
    tripped: Option<Instant>,
    /// Nothing is sent before this, after a rate limit inside the window of
    /// the last one.
    hold_until: Option<Instant>,
    /// When the budget may next step up.
    next_probe: Option<Instant>,
    /// Whether the budget has changed since it started.
    adapted: bool,
}

impl QuotaLimiter {
    pub(crate) fn new(pacing: QuotaPacing) -> Option<Self> {
        let (pacer, window) = match pacing {
            QuotaPacing::Off => return None,
            QuotaPacing::Fixed(limit) => (
                Pacer::Fixed(Bucket {
                    rate: limit.units_per_second.max(1),
                    burst: limit.burst,
                    units: f64::from(limit.burst),
                    updated: None,
                }),
                Duration::from_secs(60),
            ),
            QuotaPacing::Budget(config) => {
                let config = MinuteBudget {
                    min: config.min.max(1),
                    window: config.window.max(Duration::from_millis(1)),
                    ..config
                };
                (
                    Pacer::Budget(Budget {
                        config,
                        budget: config.max_budget(),
                        next_free: None,
                        tripped: None,
                        hold_until: None,
                        next_probe: None,
                        adapted: false,
                    }),
                    config.window,
                )
            }
        };
        Some(Self {
            state: Mutex::new(State {
                pacer,
                ledger: VecDeque::new(),
                window,
                next_id: 0,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // The state is a few numbers, valid even if a holder panicked.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current pace, budget and trailing spend.
    pub(crate) fn snapshot(&self) -> QuotaSnapshot {
        let now = Instant::now();
        let mut state = self.state();
        state.prune(now);
        let spent = state.spent(now);
        let window = state.window;
        match &state.pacer {
            Pacer::Fixed(bucket) => QuotaSnapshot {
                units_per_second: bucket.rate,
                budget: bucket.rate.saturating_mul(60).saturating_add(bucket.burst),
                spent,
            },
            Pacer::Budget(budget) => QuotaSnapshot {
                units_per_second: per_second(budget.budget, window),
                budget: budget.budget,
                spent,
            },
        }
    }

    /// The budget learned so far, if the limiter has a [`QuotaPacing::Budget`].
    pub(crate) fn learned(&self) -> Option<LearnedBudget> {
        match &self.state().pacer {
            Pacer::Budget(budget) => Some(LearnedBudget {
                units_per_minute: budget.config.units_per_minute,
                budget: budget.budget,
            }),
            Pacer::Fixed(_) => None,
        }
    }

    /// Starts the budget at `learned` (clamped to the configured range), if
    /// it was learned under the same quota and this limiter has not adapted
    /// its own budget yet. Returns whether it did.
    pub(crate) fn seed(&self, learned: LearnedBudget) -> bool {
        let mut state = self.state();
        let Pacer::Budget(budget) = &mut state.pacer else {
            return false;
        };
        if budget.adapted || learned.units_per_minute != budget.config.units_per_minute {
            return false;
        }
        budget.budget = learned
            .budget
            .clamp(budget.config.min, budget.config.max_budget());
        tracing::debug!(budget = budget.budget, "starting at a learned quota budget");
        true
    }

    /// Reserves `cost` units, and returns how long to wait before spending
    /// them and the charge's ID.
    fn reserve(&self, cost: u32) -> (Duration, u64) {
        let now = Instant::now();
        let mut guard = self.state();
        let state = &mut *guard;
        state.prune(now);
        let spent = state.spent(now);
        let at = match &mut state.pacer {
            Pacer::Fixed(bucket) => now + bucket.reserve(now, cost),
            Pacer::Budget(budget) => {
                budget.probe(now, spent);
                let start = [budget.next_free, budget.hold_until]
                    .into_iter()
                    .flatten()
                    .fold(now, Instant::max);
                let at = window_opening(&state.ledger, state.window, start, budget.budget, cost);
                budget.next_free = Some(at + spacing(cost, budget.budget, state.window));
                at
            }
        };
        let id = state.charge(at, cost);
        (at - now, id)
    }

    /// Waits until `cost` units may be spent.
    pub(crate) async fn acquire(&self, cost: u32) {
        loop {
            let (wait, id) = self.reserve(cost);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            // A rate limit while waiting cancels charges not yet sent: take
            // another turn.
            if self.state().ledger.iter().any(|charge| charge.id == id) {
                return;
            }
        }
    }

    /// Records that Gmail rate-limited a request sent at `sent`, and returns
    /// whether the budget changed.
    ///
    /// Requests sent before the last rate limit are ignored, since the old
    /// budget caused them. Otherwise charges not sent yet are cancelled. A
    /// request sent within a window of the last rate limit holds sending
    /// until that window has passed; a later one sets the budget from the
    /// trailing spend.
    pub(crate) fn rate_limited(&self, sent: Instant) -> bool {
        let now = Instant::now();
        let mut guard = self.state();
        let state = &mut *guard;
        let window = state.window;
        let Pacer::Budget(budget) = &mut state.pacer else {
            return false;
        };
        if budget.tripped.is_some_and(|at| sent < at) {
            return false;
        }
        prune(&mut state.ledger, window, now);
        state.ledger.retain(|charge| charge.at <= now);
        let spent = spent(&state.ledger, window, now);
        budget.next_free = None;
        if let Some(tripped) = budget.tripped.filter(|&at| sent < at + window) {
            let until = tripped + window;
            budget.hold_until = Some(budget.hold_until.map_or(until, |at| at.max(until)));
            budget.next_probe = Some(until + budget.config.probe_every);
            tracing::debug!(
                budget = budget.budget,
                hold_s = until.saturating_duration_since(now).as_secs_f64(),
                "Gmail rate limit again; waiting until the last one leaves the window"
            );
            return false;
        }
        let old = budget.budget;
        let learned = u64::from(spent) * u64::from(budget.config.backoff_percent.min(100)) / 100;
        budget.budget = u32::try_from(learned)
            .unwrap_or(u32::MAX)
            .min(old)
            .max(budget.config.min);
        budget.tripped = Some(now);
        budget.hold_until = None;
        budget.next_probe = Some(now + budget.config.probe_every);
        budget.adapted = true;
        tracing::debug!(
            spent,
            from = old,
            to = budget.budget,
            "Gmail rate limit; lowering the quota budget"
        );
        budget.budget != old
    }
}

impl State {
    fn prune(&mut self, now: Instant) {
        prune(&mut self.ledger, self.window, now);
    }

    fn spent(&self, now: Instant) -> u32 {
        spent(&self.ledger, self.window, now)
    }

    fn charge(&mut self, at: Instant, units: u32) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        // Each charge goes at or after the one before it, so the ledger stays
        // in order.
        self.ledger.push_back(Charge { id, at, units });
        id
    }
}

/// Forgets charges that have left the window ending at `now`.
fn prune(ledger: &mut VecDeque<Charge>, window: Duration, now: Instant) {
    while ledger
        .front()
        .is_some_and(|charge| charge.at + window <= now)
    {
        ledger.pop_front();
    }
}

/// Units charged in the window ending at `now`, not counting charges that
/// may only be sent later.
fn spent(ledger: &VecDeque<Charge>, window: Duration, now: Instant) -> u32 {
    ledger
        .iter()
        .filter(|charge| charge.at <= now && charge.at + window > now)
        .map(|charge| charge.units)
        .fold(0, u32::saturating_add)
}

impl Bucket {
    /// Takes `cost` units, going into debt if needed, and returns how long to
    /// wait before spending them.
    fn reserve(&mut self, now: Instant, cost: u32) -> Duration {
        let updated = *self.updated.get_or_insert(now);
        let elapsed = now.saturating_duration_since(updated).as_secs_f64();
        self.units = (self.units + elapsed * f64::from(self.rate)).min(f64::from(self.burst));
        self.updated = Some(now);
        self.units -= f64::from(cost);
        if self.units >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.units / f64::from(self.rate))
        }
    }
}

impl Budget {
    /// Raises the budget by a step if a busy, clean probe interval has passed.
    fn probe(&mut self, now: Instant, spent: u32) {
        let config = self.config;
        let next = *self.next_probe.get_or_insert(now + config.probe_every);
        if now < next {
            return;
        }
        self.next_probe = Some(now + config.probe_every);
        let max = config.max_budget();
        let busy = u64::from(spent) * 10 >= u64::from(self.budget) * 9;
        if !busy || self.budget >= max {
            return;
        }
        let step = (u64::from(self.budget) * u64::from(config.probe_percent) / 100).max(1);
        let old = self.budget;
        self.budget = u32::try_from(u64::from(old) + step)
            .unwrap_or(u32::MAX)
            .min(max);
        self.adapted = true;
        tracing::debug!(
            from = old,
            to = self.budget,
            "no Gmail rate limits; raising the quota budget"
        );
    }
}

/// The earliest time from `start` at which `cost` more units keep the units
/// charged in the trailing `window` within `budget`. A charge larger than the
/// budget waits for an empty window.
fn window_opening(
    ledger: &VecDeque<Charge>,
    window: Duration,
    start: Instant,
    budget: u32,
    cost: u32,
) -> Instant {
    let mut at = start;
    let mut in_window: u64 = ledger.iter().map(|charge| u64::from(charge.units)).sum();
    let cost = u64::from(cost);
    for charge in ledger {
        let leaves = charge.at + window;
        if leaves > at {
            if in_window + cost <= u64::from(budget) {
                break;
            }
            at = leaves;
        }
        in_window -= u64::from(charge.units);
    }
    at
}

/// How long `cost` units take at `budget` per `window`.
fn spacing(cost: u32, budget: u32, window: Duration) -> Duration {
    window.mul_f64(f64::from(cost) / f64::from(budget.max(1)))
}

/// `budget` per `window`, in units per second.
fn per_second(budget: u32, window: Duration) -> u32 {
    let secs = window.as_secs_f64();
    if secs <= 0.0 {
        return budget;
    }
    // Rounded; the budget is at most u32::MAX.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate = (f64::from(budget) / secs).round() as u32;
    rate
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

    fn budget(config: MinuteBudget) -> QuotaLimiter {
        QuotaLimiter::new(QuotaPacing::Budget(config)).unwrap()
    }

    /// How long a charge of `cost` waits.
    fn wait(limiter: &QuotaLimiter, cost: u32) -> Duration {
        limiter.reserve(cost).0
    }

    fn current(limiter: &QuotaLimiter) -> u32 {
        limiter.snapshot().budget
    }

    async fn advance(secs: f64) {
        tokio::time::advance(Duration::from_secs_f64(secs)).await;
    }

    fn secs(d: Duration) -> f64 {
        d.as_secs_f64()
    }

    /// Charges 40 batches of 50 units back to back from a fresh default
    /// limiter: 2,000 units over 24.375 s.
    async fn spend_2000(limiter: &QuotaLimiter) {
        for _ in 0..40 {
            limiter.acquire(50).await;
        }
    }

    #[test]
    fn the_default_budget_is_80_percent_of_6000_a_minute() {
        let config = MinuteBudget::default();
        assert_eq!(config.units_per_minute, 6_000);
        assert_eq!(config.max_budget(), 4_800);
        assert_eq!(MinuteBudget::per_minute(15_000).max_budget(), 12_000);
        // Never below the minimum.
        assert_eq!(MinuteBudget::per_minute(100).max_budget(), 600);
        assert!(QuotaLimiter::new(QuotaPacing::Off).is_none());
        assert_eq!(
            QuotaPacing::default(),
            QuotaPacing::Budget(MinuteBudget::default())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn charges_are_spread_evenly_with_no_burst_after_idle() {
        let limiter = budget(MinuteBudget::default());
        advance(300.0).await;
        // 4,800 a minute is 80 units/s: a batch of 50 every 625 ms.
        assert_eq!(wait(&limiter, 50), Duration::ZERO);
        assert_eq!(wait(&limiter, 50), Duration::from_millis(625));
        assert_eq!(wait(&limiter, 50), Duration::from_millis(1250));
        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.units_per_second, 80);
        assert_eq!(snapshot.budget, 4_800);
        // Charges count once they may be sent.
        assert_eq!(snapshot.spent, 50);
        advance(1.25).await;
        assert_eq!(limiter.snapshot().spent, 150);
    }

    #[tokio::test(start_paused = true)]
    async fn no_window_spends_more_than_the_budget() {
        let limiter = budget(MinuteBudget::default());
        let start = Instant::now();
        let mut sent = Vec::new();
        while start.elapsed() < Duration::from_secs(180) {
            limiter.acquire(50).await;
            sent.push(start.elapsed());
        }
        for (i, &at) in sent.iter().enumerate() {
            let in_window = sent[i..]
                .iter()
                .take_while(|&&t| t < at + Duration::from_secs(60))
                .count();
            assert!(in_window * 50 <= 4_800, "{in_window} batches from {at:?}");
        }
        let first_minute = sent
            .iter()
            .filter(|&&t| t < Duration::from_secs(60))
            .count();
        assert!(first_minute * 50 >= 4_700, "{first_minute}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_sets_the_budget_from_the_trailing_spend() {
        let limiter = budget(MinuteBudget::default());
        spend_2000(&limiter).await;
        assert_eq!(limiter.snapshot().spent, 2_000);

        assert!(limiter.rate_limited(Instant::now()));
        assert_eq!(current(&limiter), 1_700);
        // Nothing more until the window is back under 1,700 with room for
        // 50: the seventh batch, sent at 3.75 s, leaves at 63.75 s.
        assert_eq!(secs(wait(&limiter, 50)), 63.75 - 24.375);
        // Then 50 units every 50 * 60 / 1,700 s.
        let expected = 63.75 - 24.375 + 50.0 * 60.0 / 1_700.0;
        assert!((secs(wait(&limiter, 50)) - expected).abs() < 1e-6);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limits_on_requests_sent_before_the_last_one_are_ignored() {
        let limiter = budget(MinuteBudget::default());
        let in_flight = Instant::now();
        spend_2000(&limiter).await;
        assert!(limiter.rate_limited(Instant::now()));
        assert!(!limiter.rate_limited(in_flight));
        assert_eq!(current(&limiter), 1_700);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_within_the_last_ones_window_waits_it_out_without_lowering() {
        let limiter = budget(MinuteBudget::default());
        spend_2000(&limiter).await;
        limiter.rate_limited(Instant::now());
        // Resume at 63.75 s, and Gmail turns that away too.
        limiter.acquire(50).await;
        assert!(!limiter.rate_limited(Instant::now()));
        assert_eq!(current(&limiter), 1_700);
        // Held until a minute after the first rate limit (24.375 s).
        assert_eq!(secs(wait(&limiter, 50)), 84.375 - 63.75);
        // A rate limit after that window is a new one.
        advance(84.375 - 63.75).await;
        assert!(limiter.rate_limited(Instant::now()));
        assert_eq!(current(&limiter), 600);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_cancels_charges_not_yet_sent() {
        let limiter = std::sync::Arc::new(budget(MinuteBudget::default()));
        spend_2000(&limiter).await;
        // The next charge would go in 625 ms.
        let waiting = tokio::spawn({
            let limiter = limiter.clone();
            async move {
                limiter.acquire(50).await;
                Instant::now()
            }
        });
        tokio::task::yield_now().await;
        let tripped = Instant::now();
        limiter.rate_limited(tripped);
        assert_eq!(limiter.state().ledger.len(), 40);
        // It takes another turn, after the window has room again.
        let sent = waiting.await.unwrap();
        assert_eq!(secs(sent - tripped), 63.75 - 24.375);
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_probes_up_only_after_busy_clean_minutes() {
        let limiter = budget(MinuteBudget::default());
        assert!(limiter.seed(LearnedBudget {
            units_per_minute: 6_000,
            budget: 3_000,
        }));
        // Idle minutes do not count.
        advance(600.0).await;
        assert_eq!(wait(&limiter, 0), Duration::ZERO);
        advance(600.0).await;
        assert_eq!(current(&limiter), 3_000);
        // A busy minute raises it by 5%, and so on up to the starting budget.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(61) {
            limiter.acquire(50).await;
        }
        assert_eq!(current(&limiter), 3_150);
        while start.elapsed() < Duration::from_secs(3_600) {
            limiter.acquire(50).await;
        }
        assert_eq!(current(&limiter), 4_800);
    }

    #[tokio::test(start_paused = true)]
    async fn a_learned_budget_seeds_only_a_fresh_limiter_with_the_same_quota() {
        let limiter = budget(MinuteBudget::default());
        let learned = |units_per_minute, budget| LearnedBudget {
            units_per_minute,
            budget,
        };
        assert!(!limiter.seed(learned(15_000, 2_000)));
        assert_eq!(current(&limiter), 4_800);
        // Clamped to the configured range.
        assert!(limiter.seed(learned(6_000, 100)));
        assert_eq!(current(&limiter), 600);
        assert!(limiter.seed(learned(6_000, 9_000)));
        assert_eq!(current(&limiter), 4_800);
        assert!(limiter.seed(learned(6_000, 2_500)));
        assert_eq!(limiter.learned(), Some(learned(6_000, 2_500)));
        // Once it has learned something itself, that wins.
        spend_2000(&limiter).await;
        limiter.rate_limited(Instant::now());
        assert!(!limiter.seed(learned(6_000, 2_500)));
        assert!(fixed(QuotaLimit::default()).learned().is_none());
    }

    /// Gmail as a trailing-minute limit of `limit` units, counting every call.
    struct SlidingGmail {
        limit: u32,
        spent: VecDeque<(Instant, u32)>,
        /// With `Some`, Gmail turns everything away for this long once over
        /// the limit.
        penalty: Option<Duration>,
        locked_until: Option<Instant>,
    }

    impl SlidingGmail {
        /// Whether Gmail accepts a call of `units` sent now.
        fn call(&mut self, units: u32) -> bool {
            let now = Instant::now();
            while self
                .spent
                .front()
                .is_some_and(|(at, _)| *at + Duration::from_secs(60) <= now)
            {
                self.spent.pop_front();
            }
            self.spent.push_back((now, units));
            if self.locked_until.is_some_and(|until| now < until) {
                return false;
            }
            let total: u32 = self.spent.iter().map(|(_, units)| units).sum();
            if total <= self.limit {
                return true;
            }
            self.locked_until = self.penalty.map(|penalty| now + penalty);
            false
        }
    }

    /// Runs the default limiter against `gmail` for `minutes`, one call of 25
    /// units at a time, and returns the units accepted, the calls rejected
    /// and the lowest budget.
    async fn run_against(gmail: &mut SlidingGmail, minutes: u64) -> (u32, u32, u32) {
        let limiter = budget(MinuteBudget::default());
        let start = Instant::now();
        let (mut accepted, mut rejected, mut lowest) = (0, 0, u32::MAX);
        while start.elapsed() < Duration::from_secs(minutes * 60) {
            limiter.acquire(25).await;
            let sent = Instant::now();
            if gmail.call(25) {
                accepted += 25;
            } else {
                rejected += 1;
                limiter.rate_limited(sent);
                lowest = lowest.min(current(&limiter));
            }
        }
        (accepted, rejected, lowest)
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_settles_under_a_sliding_window_limit_without_spiralling() {
        // As measured on a real account: Gmail turned the client away once
        // about 2,750 units (by our accounting) were spent in a minute.
        let mut gmail = SlidingGmail {
            limit: 2_750,
            spent: VecDeque::new(),
            penalty: None,
            locked_until: None,
        };
        let (accepted, rejected, lowest) = run_against(&mut gmail, 15).await;
        eprintln!("sliding: {accepted} {rejected} {lowest}");
        // One rate limit to learn the ceiling, then one every few minutes as
        // the budget probes up to it again; each costs a single call.
        assert!(rejected <= 5, "{rejected} rejected");
        assert!(lowest >= 2_300, "lowest budget {lowest}");
        assert!(accepted >= 15 * 2_300, "{accepted} accepted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_minute_long_gmail_lockout_lowers_the_budget_once() {
        // Gmail turns everything away for a minute once over the limit.
        let mut gmail = SlidingGmail {
            limit: 2_750,
            spent: VecDeque::new(),
            penalty: Some(Duration::from_secs(60)),
            locked_until: None,
        };
        let (accepted, rejected, lowest) = run_against(&mut gmail, 5).await;
        eprintln!("penalty: {accepted} {rejected} {lowest}");
        assert!(rejected <= 3, "{rejected} rejected");
        assert!(lowest >= 2_300, "lowest budget {lowest}");
        assert!(accepted >= 4 * 2_300, "{accepted} accepted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_fixed_rate_ignores_rate_limits() {
        let limiter = fixed(QuotaLimit::per_second(100));
        assert!(!limiter.rate_limited(Instant::now()));
        assert_eq!(limiter.snapshot().units_per_second, 100);
        assert_eq!(wait(&limiter, 100), Duration::ZERO);
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
        assert_eq!(wait(&limiter, 500), Duration::ZERO);
        // Empty: 200 more units take 2 s to refill.
        assert_eq!(wait(&limiter, 200), Duration::from_secs(2));
        // Reservations queue: the next 100 wait behind the 200.
        assert_eq!(wait(&limiter, 100), Duration::from_secs(3));

        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(wait(&limiter, 100), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn refill_is_capped_at_the_burst() {
        let limiter = fixed(QuotaLimit {
            units_per_second: 100,
            burst: 300,
        });
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(wait(&limiter, 300), Duration::ZERO);
        assert_eq!(wait(&limiter, 100), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn calls_larger_than_the_burst_go_through_after_a_wait() {
        let limiter = fixed(QuotaLimit {
            units_per_second: 10,
            burst: 20,
        });
        assert_eq!(wait(&limiter, 50), Duration::from_secs(3));
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
