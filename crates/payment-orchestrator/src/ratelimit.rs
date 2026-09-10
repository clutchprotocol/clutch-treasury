//! Per-identity request limiting for the mutating routes.
//!
//! Keypairs are free, so "authenticated" is not a rate bound on its own: anyone willing to
//! generate a keypair and sign a challenge can reach every route here. Both POST routes cost
//! something durable. A deposit POST derives and stores an address that is then polled for the
//! life of the deployment, on a rotation with a fixed per-pass budget — so addresses nobody
//! will ever pay into slow detection down for everyone who will. A redemption POST writes an
//! intent against a payout float whose balance is the loss ceiling for the whole rail.
//!
//! Fixed window rather than a token bucket. The bound worth enforcing is "not many per minute",
//! burst behaviour at a window edge is uninteresting at these limits, and a fixed window is a
//! counter plus a timestamp instead of a rate calculation. Standard library only, deliberately:
//! this sits in a money path, and a dependency here buys nothing a `HashMap` does not already do.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most distinct identities tracked at once.
///
/// Without a bound, an attacker holding many keypairs turns the limiter itself into memory
/// exhaustion: one map entry per key, none of them ever reclaimed. Reaching this cap takes
/// `MAX_KEYS` requests inside a single window, which the per-identity limit does not prevent
/// because each request carries a different identity.
const MAX_KEYS: usize = 10_000;

struct Entry {
    window_start: Instant,
    count: u32,
}

pub struct RateLimiter {
    window: Duration,
    max_per_window: u32,
    state: Mutex<HashMap<String, Entry>>,
}

impl RateLimiter {
    pub fn new(window: Duration, max_per_window: u32) -> Self {
        Self { window, max_per_window, state: Mutex::new(HashMap::new()) }
    }

    /// Per-minute limiter, the shape every caller here wants.
    pub fn per_minute(max: u32) -> Self {
        Self::new(Duration::from_secs(60), max)
    }

    /// `Ok(())` to let the request through. `Err(retry_after)` when this identity has already
    /// used its allowance for the current window.
    ///
    /// A poisoned lock is recovered rather than propagated: the only state behind it is a
    /// request counter, so a panic elsewhere leaves nothing here that needs to be treated as
    /// corrupt, and turning that into a 500 on every later request would be a worse outage than
    /// the one that poisoned it.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(entry) = state.get_mut(key) {
            let elapsed = now.duration_since(entry.window_start);
            if elapsed >= self.window {
                entry.window_start = now;
                entry.count = 1;
                return Ok(());
            }
            if entry.count >= self.max_per_window {
                return Err(self.window - elapsed);
            }
            entry.count += 1;
            return Ok(());
        }

        // A key not seen before. Make room first, so a flood of fresh identities cannot grow
        // this map without bound.
        if state.len() >= MAX_KEYS {
            state.retain(|_, e| now.duration_since(e.window_start) < self.window);
            if state.len() >= MAX_KEYS {
                // Every entry is still inside its window. Evict the one closest to expiring
                // rather than refusing the newcomer: refusing would let an identity-churning
                // attacker lock out every real user who arrives after them, which is a worse
                // outcome than letting one attacker's counter reset early.
                let oldest = state
                    .iter()
                    .min_by_key(|(_, e)| e.window_start)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    state.remove(&k);
                }
            }
        }
        state.insert(key.to_string(), Entry { window_start: now, count: 1 });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 3);
        for i in 0..3 {
            assert!(limiter.check("0xabc").is_ok(), "request {i} is within the limit");
        }
        let retry = limiter.check("0xabc").expect_err("the fourth must be refused");
        assert!(retry <= Duration::from_secs(60) && retry > Duration::ZERO);
    }

    #[test]
    fn identities_are_counted_separately() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 1);
        assert!(limiter.check("0xaaa").is_ok());
        assert!(limiter.check("0xaaa").is_err(), "same identity is over its limit");
        assert!(limiter.check("0xbbb").is_ok(), "a different identity has its own allowance");
    }

    #[test]
    fn the_window_resets() {
        // A window short enough to wait out, so the reset is observed rather than assumed.
        let limiter = RateLimiter::new(Duration::from_millis(50), 1);
        assert!(limiter.check("0xabc").is_ok());
        assert!(limiter.check("0xabc").is_err());
        std::thread::sleep(Duration::from_millis(60));
        assert!(limiter.check("0xabc").is_ok(), "a new window starts with a fresh count");
    }

    #[test]
    fn distinct_keys_stay_bounded() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 1);
        for i in 0..(MAX_KEYS + 500) {
            assert!(limiter.check(&format!("0x{i}")).is_ok(), "a fresh identity is never refused");
        }
        let held = limiter.state.lock().unwrap().len();
        assert!(held <= MAX_KEYS, "map grew past its cap: {held}");
    }
}
