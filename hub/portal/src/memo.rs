//! Short-lived memos for values that many requests would otherwise rebuild
//! from the database (MR !103 review).
//!
//! Both users rebuild the same answer for every request: the peak of an
//! ended alarm on each `/alarms` render, and the page-stats row that every
//! open tab refetches on the same SSE nudge. A memo holds each answer for a
//! bounded time instead of forever, so a value that can still change (a
//! backfill landing readings inside an ended alarm's window, a Hub going
//! offline) is at most one TTL stale.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A map whose entries expire `ttl` after they were inserted. Bounded by
/// `cap`: an insert into a full map first drops the expired entries, and
/// clears the map if that frees nothing.
pub struct TtlMemo<K, V> {
    ttl: Duration,
    cap: usize,
    entries: Mutex<HashMap<K, (Instant, V)>>,
}

impl<K: Eq + Hash, V: Clone> TtlMemo<K, V> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The value stored for `key`, if it has not expired.
    pub fn get(&self, key: &K) -> Option<V> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(key)
            .filter(|(at, _)| at.elapsed() < self.ttl)
            .map(|(_, v)| v.clone())
    }

    pub fn insert(&self, key: K, value: V) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= self.cap && !entries.contains_key(&key) {
            entries.retain(|_, (at, _)| at.elapsed() < self.ttl);
            if entries.len() >= self.cap {
                entries.clear();
            }
        }
        entries.insert(key, (Instant::now(), value));
    }
}

/// The memos on [`crate::http::AppState`].
pub struct RenderMemos {
    /// Alarm id -> the peak of an alarm that has ended. Its window is fixed
    /// once it ends, so the peak only moves if a backfill lands readings in
    /// it; five minutes bounds that. An open alarm is never memoised.
    pub alarm_peaks: TtlMemo<String, Option<crate::render::AlarmPeak>>,
    /// `/stats/{scope}` slug -> the built row. The row holds i18n keys, not
    /// text, so one build serves every viewer and language. A second is
    /// shorter than the SSE nudge window, and long enough that the tabs one
    /// nudge reaches share one build.
    pub page_stats: TtlMemo<String, crate::render::PageStatsCtx>,
}

impl Default for RenderMemos {
    fn default() -> Self {
        Self {
            alarm_peaks: TtlMemo::new(Duration::from_secs(300), 4096),
            page_stats: TtlMemo::new(Duration::from_secs(1), 16),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_expires_after_its_ttl() {
        let memo = TtlMemo::new(Duration::from_millis(50), 8);
        memo.insert("a", 1);
        assert_eq!(memo.get(&"a"), Some(1));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(memo.get(&"a"), None);
    }

    #[test]
    fn a_full_memo_makes_room() {
        let memo = TtlMemo::new(Duration::from_secs(60), 2);
        memo.insert("a", 1);
        memo.insert("b", 2);
        memo.insert("a", 3);
        assert_eq!(memo.get(&"b"), Some(2), "replacing a key needs no room");
        memo.insert("c", 4);
        assert_eq!(memo.get(&"c"), Some(4));
        assert_eq!(memo.get(&"a"), None, "nothing had expired, so it cleared");
    }
}
