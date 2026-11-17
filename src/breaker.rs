//! Per-backend circuit breaker.
//!
//! Closed: traffic flows. After `threshold` consecutive failures it opens for `open_for`.
//! When the open period ends it lets one trial request through (half-open). A trial success
//! closes it. A trial failure reopens it for twice as long, up to `max_open`. That doubling is
//! what keeps a dead backend from receiving a retry storm.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
enum Phase {
    Closed,
    Open { until: Instant },
    HalfOpen { trial_since: Option<Instant> },
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
    failures: u32,
    open_for: Duration,
    opened_total: u64,
}

#[derive(Debug)]
pub struct Breaker {
    inner: Mutex<Inner>,
    threshold: u32,
    base_open: Duration,
    max_open: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

impl Breaker {
    pub fn new(threshold: u32, base_open: Duration, max_open: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                phase: Phase::Closed,
                failures: 0,
                open_for: base_open,
                opened_total: 0,
            }),
            threshold: threshold.max(1),
            base_open,
            max_open: max_open.max(base_open),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means another thread panicked mid-update; the state is still usable.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// May a request go to this backend now? Half-open admits one trial at a time.
    pub fn allow(&self, now: Instant) -> bool {
        let mut g = self.lock();
        match g.phase {
            Phase::Closed => true,
            Phase::Open { until } if now >= until => {
                g.phase = Phase::HalfOpen {
                    trial_since: Some(now),
                };
                true
            }
            Phase::Open { .. } => false,
            Phase::HalfOpen { trial_since } => match trial_since {
                None => {
                    g.phase = Phase::HalfOpen {
                        trial_since: Some(now),
                    };
                    true
                }
                // A trial that never reported (client gone) must not wedge the circuit.
                Some(t) if now.duration_since(t) >= g.open_for => {
                    g.phase = Phase::HalfOpen {
                        trial_since: Some(now),
                    };
                    true
                }
                Some(_) => false,
            },
        }
    }

    pub fn success(&self) {
        let mut g = self.lock();
        g.phase = Phase::Closed;
        g.failures = 0;
        g.open_for = self.base_open;
    }

    pub fn failure(&self, now: Instant) {
        let mut g = self.lock();
        match g.phase {
            Phase::Closed => {
                g.failures += 1;
                if g.failures >= self.threshold {
                    let until = now + g.open_for;
                    g.phase = Phase::Open { until };
                    g.opened_total += 1;
                }
            }
            Phase::HalfOpen { .. } => {
                g.open_for = (g.open_for * 2).min(self.max_open);
                let until = now + g.open_for;
                g.phase = Phase::Open { until };
                g.opened_total += 1;
            }
            Phase::Open { .. } => {}
        }
    }

    /// Forget past failures. Used when the health probe sees the backend come back.
    pub fn reset(&self) {
        self.success();
    }

    pub fn state(&self, now: Instant) -> CircuitState {
        match self.lock().phase {
            Phase::Closed => CircuitState::Closed,
            Phase::Open { until } if now >= until => CircuitState::HalfOpen,
            Phase::Open { .. } => CircuitState::Open,
            Phase::HalfOpen { .. } => CircuitState::HalfOpen,
        }
    }

    pub fn opened_total(&self) -> u64 {
        self.lock().opened_total
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.lock().failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn br() -> Breaker {
        Breaker::new(3, Duration::from_secs(10), Duration::from_secs(40))
    }

    #[test]
    fn opens_after_threshold_and_blocks() {
        let b = br();
        let t0 = Instant::now();
        b.failure(t0);
        b.failure(t0);
        assert!(b.allow(t0));
        b.failure(t0);
        assert_eq!(b.state(t0), CircuitState::Open);
        assert!(!b.allow(t0 + Duration::from_secs(9)));
    }

    #[test]
    fn half_open_admits_exactly_one_trial() {
        let b = br();
        let t0 = Instant::now();
        for _ in 0..3 {
            b.failure(t0);
        }
        let t1 = t0 + Duration::from_secs(11);
        assert_eq!(b.state(t1), CircuitState::HalfOpen);
        assert!(b.allow(t1), "first trial passes");
        assert!(!b.allow(t1), "second concurrent trial is held back");
    }

    #[test]
    fn failed_trial_doubles_open_period_up_to_cap() {
        let b = br();
        let mut now = Instant::now();
        for _ in 0..3 {
            b.failure(now);
        }
        let mut expect = 10u64;
        for _ in 0..4 {
            now += Duration::from_secs(expect + 1);
            assert!(b.allow(now));
            b.failure(now);
            expect = (expect * 2).min(40);
            assert!(
                !b.allow(now + Duration::from_secs(expect - 1)),
                "still open for {expect}s"
            );
        }
        assert_eq!(expect, 40);
    }

    #[test]
    fn trial_success_closes_and_resets_period() {
        let b = br();
        let t0 = Instant::now();
        for _ in 0..3 {
            b.failure(t0);
        }
        let t1 = t0 + Duration::from_secs(11);
        assert!(b.allow(t1));
        b.success();
        assert_eq!(b.state(t1), CircuitState::Closed);
        assert_eq!(b.consecutive_failures(), 0);
        for _ in 0..3 {
            b.failure(t1);
        }
        assert!(
            b.allow(t1 + Duration::from_secs(11)),
            "period reset to base after success"
        );
    }

    #[test]
    fn success_in_closed_clears_failure_streak() {
        let b = br();
        let t0 = Instant::now();
        b.failure(t0);
        b.failure(t0);
        b.success();
        b.failure(t0);
        assert_eq!(b.state(t0), CircuitState::Closed);
    }
}
