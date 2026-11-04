//! Attempt throttle for the unauthenticated forms (LE-11, local E2E 2026-09-26): 15 wrong
//! passwords in 9 s went through, where v1 allowed 10 login and 5 setup attempts a minute per
//! client. In memory; a restart forgets it, which costs an attacker a restart they cannot
//! cause.
//!
//! An attempt is counted when it starts, under the same lock that checks the limit, so
//! parallel requests cannot all pass the check before any of them is counted (R2-1, review
//! 2 of !102).
//!
//! A full window locks the key, and each lockout lasts twice the one before, up to
//! [`MAX_LOCKOUT_WINDOWS`] windows (#103): a fixed window still let a client guess the limit
//! every window, forever. The doubling is forgotten once the key has gone that long again
//! without a lockout.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Keys kept before stale ones are swept, so a scan from many addresses cannot grow the map
/// without bound.
const SWEEP_AT: usize = 4096;

/// The longest lockout, in windows: 1, 2, 4, 8, then 16 for every lockout after.
const MAX_LOCKOUT_WINDOWS: u32 = 16;

/// A refused attempt. `first` is true for the first refusal of a lockout, so a caller can
/// audit the lockout once instead of once per request (R2-3). `retry_after` is what is left
/// of the lockout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    pub first: bool,
    pub retry_after: Duration,
}

#[derive(Default)]
struct Entry {
    times: VecDeque<Instant>,
    /// Lockouts so far; the next one lasts `2^strikes` windows.
    strikes: u32,
    locked_until: Option<Instant>,
}

impl Entry {
    /// Whether the entry still holds anything at `now`: a recent attempt, a lockout, or
    /// strikes that have not decayed yet.
    fn live(&self, now: Instant, window: Duration, max_lockout: Duration) -> bool {
        self.times
            .back()
            .is_some_and(|t| now.duration_since(*t) < window)
            || self
                .locked_until
                .is_some_and(|until| now < until + max_lockout)
    }
}

pub struct Throttle {
    limit: usize,
    window: Duration,
    hits: Mutex<HashMap<String, Entry>>,
}

impl Throttle {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    fn max_lockout(&self) -> Duration {
        self.window * MAX_LOCKOUT_WINDOWS
    }

    /// Counts one attempt against `key`, or refuses it while the key is locked. The attempt
    /// that finds `limit` attempts inside the window locks the key.
    pub fn try_acquire(&self, key: &str) -> Result<(), Refused> {
        let now = Instant::now();
        let window = self.window;
        let max_lockout = self.max_lockout();
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if hits.len() >= SWEEP_AT && !hits.contains_key(key) {
            hits.retain(|_, e| e.live(now, window, max_lockout));
        }
        let entry = hits.entry(key.to_string()).or_default();
        if let Some(until) = entry.locked_until {
            if now < until {
                return Err(Refused {
                    first: false,
                    retry_after: until - now,
                });
            }
            if now >= until + max_lockout {
                entry.strikes = 0;
                entry.locked_until = None;
            }
        }
        while entry
            .times
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            entry.times.pop_front();
        }
        if entry.times.len() >= self.limit {
            let lockout = (window * 2u32.saturating_pow(entry.strikes)).min(max_lockout);
            entry.strikes = entry.strikes.saturating_add(1);
            entry.locked_until = Some(now + lockout);
            entry.times.clear();
            return Err(Refused {
                first: true,
                retry_after: lockout,
            });
        }
        entry.times.push_back(now);
        Ok(())
    }

    /// Takes back the newest attempt of `key`: a successful login is not a failure to count.
    pub fn release(&self, key: &str) {
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = hits.get_mut(key) {
            entry.times.pop_back();
        }
    }

    /// Forgets `key`'s attempts and its lockouts.
    pub fn clear(&self, key: &str) {
        self.hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_holds_per_key_and_the_lockout_releases_it() {
        let window = Duration::from_millis(200);
        let throttle = Throttle::new(3, window);
        for _ in 0..3 {
            assert!(throttle.try_acquire("a").is_ok());
        }
        assert_eq!(
            throttle.try_acquire("a"),
            Err(Refused {
                first: true,
                retry_after: window
            })
        );
        let again = throttle.try_acquire("a").unwrap_err();
        assert!(!again.first, "the lockout is noted once");
        assert!(again.retry_after <= window, "{again:?}");
        assert!(
            throttle.try_acquire("b").is_ok(),
            "another key is not affected"
        );

        std::thread::sleep(Duration::from_millis(250));
        assert!(throttle.try_acquire("a").is_ok(), "the lockout released it");

        throttle.clear("a");
        for _ in 0..3 {
            assert!(throttle.try_acquire("a").is_ok());
        }
        throttle.release("a");
        assert!(
            throttle.try_acquire("a").is_ok(),
            "a released attempt is free again"
        );
    }

    /// #103: each lockout lasts twice the one before, up to the cap.
    #[test]
    fn lockouts_double_up_to_the_cap() {
        let window = Duration::from_millis(1);
        let throttle = Throttle::new(1, window);
        let mut lockouts = Vec::new();
        for _ in 0..7 {
            assert!(throttle.try_acquire("a").is_ok());
            let refused = throttle.try_acquire("a").unwrap_err();
            assert!(refused.first);
            lockouts.push(refused.retry_after);
            std::thread::sleep(refused.retry_after + Duration::from_millis(1));
        }
        let ms = [1, 2, 4, 8, 16, 16, 16].map(Duration::from_millis);
        assert_eq!(lockouts, ms);
    }

    /// #103: a key that goes the longest lockout again without a lockout starts over, and a
    /// cleared key (a successful login) starts over at once.
    #[test]
    fn strikes_decay_and_clear_resets_them() {
        let window = Duration::from_millis(5);
        let throttle = Throttle::new(1, window);
        let lock = |throttle: &Throttle| {
            assert!(throttle.try_acquire("a").is_ok());
            throttle.try_acquire("a").unwrap_err().retry_after
        };
        assert_eq!(lock(&throttle), window);
        std::thread::sleep(window + Duration::from_millis(2));
        assert_eq!(lock(&throttle), window * 2, "a second lockout is longer");

        std::thread::sleep(window * 2 + window * MAX_LOCKOUT_WINDOWS + Duration::from_millis(5));
        assert_eq!(lock(&throttle), window, "the strikes decayed");

        throttle.clear("a");
        assert_eq!(lock(&throttle), window, "clear forgets the strikes");
    }

    /// R2-1: attempts are counted as they start, so parallel callers cannot all pass.
    #[test]
    fn parallel_attempts_cannot_overrun_the_limit() {
        let throttle = std::sync::Arc::new(Throttle::new(10, Duration::from_secs(60)));
        let admitted: usize = (0..50)
            .map(|_| {
                let throttle = std::sync::Arc::clone(&throttle);
                std::thread::spawn(move || usize::from(throttle.try_acquire("ip").is_ok()))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum();
        assert_eq!(admitted, 10);
    }

    #[test]
    fn stale_keys_are_swept_before_the_map_grows_past_the_bound() {
        let throttle = Throttle::new(1, Duration::from_millis(1));
        for i in 0..SWEEP_AT {
            let _ = throttle.try_acquire(&format!("k{i}"));
        }
        std::thread::sleep(Duration::from_millis(5));
        let _ = throttle.try_acquire("new");
        assert_eq!(throttle.hits.lock().unwrap().len(), 1);
    }
}
