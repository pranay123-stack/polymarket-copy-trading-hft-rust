//! Reconnection policy: exponential backoff with full jitter, and a circuit breaker.
//!
//! Jitter is not decorative. Without it, every reconnecting client retries in lockstep
//! after an outage and hammers the venue at exactly the moment it is recovering.

use std::time::Duration;

/// Exponential backoff with full jitter, capped.
#[derive(Debug, Clone)]
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
    factor: u32,
    attempt: u32,
    /// Deterministic PRNG state — reproducible in tests, still well-spread in practice.
    seed: u64,
}

impl Default for Backoff {
    fn default() -> Self { Self::new(500, 30_000) }
}

impl Backoff {
    pub fn new(base_ms: u64, max_ms: u64) -> Self {
        Self { base_ms, max_ms, factor: 2, attempt: 0, seed: 0x2545F491_4F6CDD1D }
    }

    pub fn attempt(&self) -> u32 { self.attempt }

    /// Clears the backoff after a successful connection.
    pub fn reset(&mut self) { self.attempt = 0; }

    /// The delay before the next attempt, with full jitter: `rand(0, min(max, base*2^n))`.
    pub fn next_delay(&mut self) -> Duration {
        let exp = self
            .base_ms
            .saturating_mul((self.factor as u64).saturating_pow(self.attempt.min(16)));
        let ceiling = exp.min(self.max_ms).max(1);
        self.attempt = self.attempt.saturating_add(1);
        // xorshift64*
        self.seed ^= self.seed >> 12;
        self.seed ^= self.seed << 25;
        self.seed ^= self.seed >> 27;
        let r = self.seed.wrapping_mul(0x2545F4914F6CDD1D);
        Duration::from_millis(r % ceiling)
    }

    /// The un-jittered ceiling, for assertions and logging.
    pub fn ceiling_ms(&self) -> u64 {
        self.base_ms
            .saturating_mul((self.factor as u64).saturating_pow(self.attempt.min(16)))
            .min(self.max_ms)
    }
}

/// Trips after repeated failures so a hard-down dependency stops being retried on the
/// hot path, and probes for recovery after a cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    failure_threshold: u32,
    consecutive_failures: u32,
    cooldown: Duration,
    state: BreakerState,
    opened_for_ms: u64,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            consecutive_failures: 0,
            cooldown,
            state: BreakerState::Closed,
            opened_for_ms: 0,
        }
    }

    pub fn state(&self) -> BreakerState { self.state }
    pub fn is_open(&self) -> bool { self.state == BreakerState::Open }

    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = BreakerState::Closed;
        self.opened_for_ms = 0;
    }

    pub fn on_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= self.failure_threshold {
            self.state = BreakerState::Open;
            self.opened_for_ms = 0;
        }
    }

    /// Advances the cooldown clock; call it with elapsed time between attempts.
    pub fn tick(&mut self, elapsed: Duration) {
        if self.state == BreakerState::Open {
            self.opened_for_ms += elapsed.as_millis() as u64;
            if self.opened_for_ms >= self.cooldown.as_millis() as u64 {
                // Let exactly one probe through.
                self.state = BreakerState::HalfOpen;
            }
        }
    }

    /// May a request be attempted now?
    pub fn allows_request(&self) -> bool {
        matches!(self.state, BreakerState::Closed | BreakerState::HalfOpen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_ceiling_grows_then_caps() {
        let mut b = Backoff::new(500, 8_000);
        let mut ceilings = vec![];
        for _ in 0..8 {
            ceilings.push(b.ceiling_ms());
            b.next_delay();
        }
        assert_eq!(&ceilings[..5], &[500, 1000, 2000, 4000, 8000]);
        assert!(ceilings.iter().all(|c| *c <= 8_000), "must never exceed the cap");
    }

    #[test]
    fn jitter_keeps_delays_inside_the_ceiling() {
        let mut b = Backoff::new(500, 30_000);
        for _ in 0..50 {
            let ceiling = b.ceiling_ms();
            let d = b.next_delay().as_millis() as u64;
            assert!(d < ceiling.max(1), "delay {d} exceeded ceiling {ceiling}");
        }
    }

    #[test]
    fn jitter_actually_varies() {
        // Identical delays would defeat the point: a thundering herd on recovery.
        let mut b = Backoff::new(1000, 30_000);
        for _ in 0..6 { b.next_delay(); }
        let s: std::collections::HashSet<u128> =
            (0..12).map(|_| b.next_delay().as_millis()).collect();
        assert!(s.len() > 6, "expected spread-out delays, got {s:?}");
    }

    #[test]
    fn reset_returns_to_the_base_delay() {
        let mut b = Backoff::new(500, 30_000);
        for _ in 0..6 { b.next_delay(); }
        assert!(b.ceiling_ms() > 500);
        b.reset();
        assert_eq!(b.ceiling_ms(), 500);
        assert_eq!(b.attempt(), 0);
    }

    #[test]
    fn breaker_opens_only_after_the_threshold() {
        let mut c = CircuitBreaker::new(3, Duration::from_secs(5));
        c.on_failure();
        c.on_failure();
        assert_eq!(c.state(), BreakerState::Closed);
        assert!(c.allows_request());
        c.on_failure();
        assert_eq!(c.state(), BreakerState::Open);
        assert!(!c.allows_request(), "open breaker must block requests");
    }

    #[test]
    fn success_resets_the_failure_run() {
        let mut c = CircuitBreaker::new(3, Duration::from_secs(5));
        c.on_failure();
        c.on_failure();
        c.on_success();
        c.on_failure();
        assert_eq!(c.state(), BreakerState::Closed, "failures must be consecutive to trip");
    }

    #[test]
    fn breaker_half_opens_after_cooldown_and_closes_on_success() {
        let mut c = CircuitBreaker::new(1, Duration::from_secs(5));
        c.on_failure();
        assert!(c.is_open());
        c.tick(Duration::from_secs(2));
        assert!(c.is_open(), "must stay open until the cooldown elapses");
        c.tick(Duration::from_secs(4));
        assert_eq!(c.state(), BreakerState::HalfOpen);
        assert!(c.allows_request(), "one probe must be allowed through");
        c.on_success();
        assert_eq!(c.state(), BreakerState::Closed);
    }
}
