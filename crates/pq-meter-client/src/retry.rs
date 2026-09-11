//! Backoff for the two things on the gateway that fail and recover: the meter and the link.
//!
//! Both need the same shape — wait longer after each failure, up to a ceiling, and start
//! over once something succeeds. The wait is jittered so that a fleet of gateways coming
//! back after a shared outage does not retry in lockstep and knock the receiver over again.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Jitter applied to every delay, as a fraction either side of the nominal wait.
const JITTER_NUMERATOR: u64 = 250;
const JITTER_DENOMINATOR: u64 = 1000;

/// Exponentially increasing, jittered delays between retries.
pub struct Backoff {
    base: Duration,
    max: Duration,
    next: Duration,
    rng: XorShift64,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self::with_seed(base, max, seed_from_clock())
    }

    /// A backoff with a fixed random seed, so tests see the same sequence every run.
    pub fn with_seed(base: Duration, max: Duration, seed: u64) -> Self {
        let base = base.max(Duration::from_millis(1));
        Self {
            base,
            max: max.max(base),
            next: base,
            rng: XorShift64::new(seed),
        }
    }

    /// The delay to wait before the next attempt, then doubles it for the attempt after.
    pub fn next_delay(&mut self) -> Duration {
        let nominal = self.next;
        self.next = (nominal * 2).min(self.max);
        self.jitter(nominal)
    }

    /// Goes back to the shortest delay. Call this after anything succeeds.
    pub fn reset(&mut self) {
        self.next = self.base;
    }

    /// Spreads a delay over ±25 % of its nominal value.
    fn jitter(&mut self, delay: Duration) -> Duration {
        let millis = delay.as_millis().min(u128::from(u64::MAX)) as u64;
        let span = 2 * JITTER_NUMERATOR + 1;
        let factor = JITTER_DENOMINATOR - JITTER_NUMERATOR + self.rng.next() % span;
        Duration::from_millis(millis.saturating_mul(factor) / JITTER_DENOMINATOR)
    }
}

/// A tiny non-cryptographic PRNG, enough to spread retries apart.
///
/// Jitter is the only thing the gateway needs randomness for, and it does not need to be
/// unpredictable — which is worth a few lines here rather than a dependency that has to
/// cross-compile to the Pi.
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // Zero is the one state this generator cannot leave.
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }
}

fn seed_from_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(30);

    /// Every delay stays within ±25 % of the nominal one, which doubles each time.
    #[test]
    fn delays_double_and_stay_within_the_jitter_band() {
        let mut backoff = Backoff::with_seed(BASE, MAX, 12345);
        let mut nominal = BASE;
        for _ in 0..8 {
            let delay = backoff.next_delay();
            let low = nominal.mul_f64(0.75);
            let high = nominal.mul_f64(1.25);
            assert!(
                delay >= low && delay <= high,
                "{delay:?} outside {low:?}..={high:?}"
            );
            nominal = (nominal * 2).min(MAX);
        }
    }

    #[test]
    fn delays_never_exceed_the_ceiling_plus_its_jitter() {
        let mut backoff = Backoff::with_seed(BASE, MAX, 999);
        let mut longest = Duration::ZERO;
        for _ in 0..50 {
            longest = longest.max(backoff.next_delay());
        }
        assert!(
            longest <= MAX.mul_f64(1.25),
            "{longest:?} exceeds the ceiling"
        );
        assert!(
            longest > MAX.mul_f64(0.5),
            "the ceiling was never approached"
        );
    }

    #[test]
    fn reset_goes_back_to_the_shortest_delay() {
        let mut backoff = Backoff::with_seed(BASE, MAX, 7);
        for _ in 0..5 {
            backoff.next_delay();
        }
        backoff.reset();
        assert!(backoff.next_delay() <= BASE.mul_f64(1.25));
    }

    /// Two gateways that fail at the same moment must not retry at the same moment.
    #[test]
    fn different_seeds_produce_different_delays() {
        let mut one = Backoff::with_seed(BASE, MAX, 1);
        let mut two = Backoff::with_seed(BASE, MAX, 2);
        let differs = (0..8).any(|_| one.next_delay() != two.next_delay());
        assert!(differs, "jitter is identical across seeds");
    }

    #[test]
    fn a_zero_seed_still_generates_a_sequence() {
        let mut rng = XorShift64::new(0);
        assert_ne!(rng.next(), 0);
        assert_ne!(rng.next(), 0);
    }
}
