//! How many batch requests a client keeps in flight, fixed or adapted to
//! Gmail's rate limiting.
//!
//! Gmail throttles each user's concurrent requests ("too many concurrent
//! requests for user") well before the quota limiter's unit budget runs out, and
//! the safe level varies by account and over time. The adaptive mode is an AIMD
//! controller, like TCP congestion control: it starts low, adds one batch in
//! flight after a run of clean batches, and halves when a batch comes back with
//! rate-limited items. The quota limiter stays the hard cap on the overall rate.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// Batches in flight when the concurrency is adaptive: where it starts, how
/// far it may grow, and how quickly it moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveConcurrency {
    /// Batches in flight to start with.
    pub initial: usize,
    /// The most batches in flight.
    pub max: usize,
    /// Clean batches (no rate-limited items) in a row before one more batch
    /// may be in flight.
    pub increase_after: u32,
    /// After halving, how long to ignore further results: batches already in
    /// flight were sent at the old level, so their rate limits should not halve
    /// it again, and their successes should not count towards growing it.
    pub cooldown: Duration,
}

impl Default for AdaptiveConcurrency {
    /// Start at 2, grow to at most 6, one step per 4 clean batches, with a
    /// 3-second cooldown after halving.
    fn default() -> Self {
        Self {
            initial: 2,
            max: 6,
            increase_after: 4,
            cooldown: Duration::from_secs(3),
        }
    }
}

/// How many batch requests a client keeps in flight at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchConcurrency {
    /// Always this many (at least 1).
    Fixed(usize),
    /// Adapted to Gmail's rate limiting.
    Adaptive(AdaptiveConcurrency),
}

impl Default for BatchConcurrency {
    fn default() -> Self {
        Self::Adaptive(AdaptiveConcurrency::default())
    }
}

/// The additive-increase, multiplicative-decrease rule, on its own so it can
/// be tested with explicit times.
#[derive(Debug, Clone)]
pub(crate) struct Aimd {
    config: AdaptiveConcurrency,
    limit: usize,
    /// Clean batches since the last change.
    clean: u32,
    cooldown_until: Option<Instant>,
}

impl Aimd {
    pub(crate) fn new(config: AdaptiveConcurrency) -> Self {
        let max = config.max.max(1);
        let config = AdaptiveConcurrency { max, ..config };
        Self {
            config,
            limit: config.initial.clamp(1, max),
            clean: 0,
            cooldown_until: None,
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    /// Records one batch's outcome at `now`. Returns the new limit if it
    /// changed.
    pub(crate) fn record(&mut self, rate_limited: bool, now: Instant) -> Option<usize> {
        if self.cooldown_until.is_some_and(|until| now < until) {
            return None;
        }
        self.cooldown_until = None;
        let old = self.limit;
        if rate_limited {
            self.limit = (self.limit / 2).max(1);
            self.clean = 0;
            self.cooldown_until = Some(now + self.config.cooldown);
        } else {
            self.clean += 1;
            if self.clean >= self.config.increase_after.max(1) && self.limit < self.config.max {
                self.limit += 1;
                self.clean = 0;
            }
        }
        (self.limit != old).then_some(self.limit)
    }
}

/// Admits batch requests up to the current limit, shared by a client's clones.
#[derive(Debug)]
pub(crate) struct ConcurrencyGate {
    state: Mutex<GateState>,
    /// Woken when a permit is released or the limit grows.
    released: Notify,
}

#[derive(Debug)]
struct GateState {
    in_flight: usize,
    fixed: usize,
    /// `None` for a fixed limit.
    aimd: Option<Aimd>,
}

impl GateState {
    fn limit(&self) -> usize {
        self.aimd.as_ref().map_or(self.fixed, Aimd::limit)
    }
}

impl ConcurrencyGate {
    pub(crate) fn new(concurrency: BatchConcurrency) -> Self {
        let (fixed, aimd) = match concurrency {
            BatchConcurrency::Fixed(n) => (n.max(1), None),
            BatchConcurrency::Adaptive(config) => (1, Some(Aimd::new(config))),
        };
        Self {
            state: Mutex::new(GateState {
                in_flight: 0,
                fixed,
                aimd,
            }),
            released: Notify::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        // The state is a few counters, valid even if a holder panicked.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current limit on batches in flight.
    pub(crate) fn limit(&self) -> usize {
        self.state().limit()
    }

    /// Waits until another batch may be sent. The batch counts as in flight
    /// until the permit is dropped.
    pub(crate) async fn acquire(&self) -> Permit<'_> {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            // Register before checking, so a release in between is not missed.
            released.as_mut().enable();
            {
                let mut state = self.state();
                if state.in_flight < state.limit() {
                    state.in_flight += 1;
                    return Permit { gate: self };
                }
            }
            released.await;
        }
    }

    /// Records a batch's outcome: whether any of its items were rate-limited.
    /// A fixed limit ignores it.
    pub(crate) fn record(&self, rate_limited: bool) {
        let mut state = self.state();
        let old = state.limit();
        let Some(new) = state
            .aimd
            .as_mut()
            .and_then(|aimd| aimd.record(rate_limited, Instant::now()))
        else {
            return;
        };
        drop(state);
        if new < old {
            tracing::debug!(
                from = old,
                to = new,
                "Gmail rate-limited a batch; fewer batches in flight"
            );
        } else {
            tracing::debug!(
                from = old,
                to = new,
                "clean Gmail batches; more batches in flight"
            );
            self.released.notify_waiters();
        }
    }
}

/// One batch in flight; dropping it lets another start.
#[derive(Debug)]
pub(crate) struct Permit<'a> {
    gate: &'a ConcurrencyGate,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.gate.state().in_flight -= 1;
        self.gate.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn config() -> AdaptiveConcurrency {
        AdaptiveConcurrency {
            initial: 2,
            max: 6,
            increase_after: 3,
            cooldown: Duration::from_secs(3),
        }
    }

    fn secs(start: Instant, s: u64) -> Instant {
        start + Duration::from_secs(s)
    }

    #[test]
    fn grows_by_one_after_a_run_of_clean_batches_up_to_the_max() {
        let start = Instant::now();
        let mut aimd = Aimd::new(config());
        assert_eq!(aimd.limit(), 2);
        let mut changes = Vec::new();
        for _ in 0..20 {
            changes.extend(aimd.record(false, start));
        }
        assert_eq!(changes, [3, 4, 5, 6]);
        assert_eq!(aimd.limit(), 6);
    }

    #[test]
    fn halves_on_a_rate_limit_down_to_one() {
        let start = Instant::now();
        let mut aimd = Aimd::new(AdaptiveConcurrency {
            initial: 6,
            ..config()
        });
        assert_eq!(aimd.record(true, secs(start, 0)), Some(3));
        assert_eq!(aimd.record(true, secs(start, 3)), Some(1));
        assert_eq!(aimd.record(true, secs(start, 6)), None);
        assert_eq!(aimd.limit(), 1);
    }

    #[test]
    fn ignores_results_during_the_cooldown() {
        let start = Instant::now();
        let mut aimd = Aimd::new(AdaptiveConcurrency {
            initial: 6,
            ..config()
        });
        assert_eq!(aimd.record(true, secs(start, 0)), Some(3));
        // Batches sent before the decrease come back: neither halves again nor
        // counts as clean.
        assert_eq!(aimd.record(true, secs(start, 1)), None);
        for _ in 0..5 {
            assert_eq!(aimd.record(false, secs(start, 2)), None);
        }
        assert_eq!(aimd.limit(), 3);
        // After the cooldown, clean batches count from zero.
        assert_eq!(aimd.record(false, secs(start, 3)), None);
        assert_eq!(aimd.record(false, secs(start, 3)), None);
        assert_eq!(aimd.record(false, secs(start, 3)), Some(4));
    }

    #[test]
    fn a_rate_limit_resets_the_clean_run() {
        let start = Instant::now();
        let mut aimd = Aimd::new(AdaptiveConcurrency {
            cooldown: Duration::ZERO,
            ..config()
        });
        aimd.record(false, start);
        aimd.record(false, start);
        assert_eq!(aimd.record(true, start), Some(1));
        aimd.record(false, start);
        aimd.record(false, start);
        assert_eq!(aimd.record(false, start), Some(2));
    }

    #[test]
    fn out_of_range_settings_are_clamped() {
        let aimd = Aimd::new(AdaptiveConcurrency {
            initial: 10,
            max: 0,
            increase_after: 0,
            cooldown: Duration::ZERO,
        });
        assert_eq!(aimd.limit(), 1);
        let aimd = Aimd::new(AdaptiveConcurrency {
            initial: 0,
            ..config()
        });
        assert_eq!(aimd.limit(), 1);
    }

    fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[tokio::test]
    async fn the_gate_admits_up_to_the_limit() {
        let gate = ConcurrencyGate::new(BatchConcurrency::Fixed(2));
        let a = gate.acquire().await;
        let _b = gate.acquire().await;
        let mut third = pin!(gate.acquire());
        assert!(poll_once(third.as_mut()).is_pending());
        drop(a);
        assert!(poll_once(third.as_mut()).is_ready());
    }

    #[tokio::test]
    async fn a_growing_limit_admits_waiting_batches() {
        let gate = ConcurrencyGate::new(BatchConcurrency::Adaptive(AdaptiveConcurrency {
            initial: 1,
            max: 2,
            increase_after: 1,
            cooldown: Duration::ZERO,
        }));
        let _a = gate.acquire().await;
        let mut second = pin!(gate.acquire());
        assert!(poll_once(second.as_mut()).is_pending());
        gate.record(false);
        assert_eq!(gate.limit(), 2);
        assert!(poll_once(second.as_mut()).is_ready());
    }

    #[tokio::test]
    async fn a_fixed_gate_ignores_results() {
        let gate = ConcurrencyGate::new(BatchConcurrency::Fixed(4));
        gate.record(true);
        assert_eq!(gate.limit(), 4);
    }
}
