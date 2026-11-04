//! Per-central response buffers — `plan/control/ble/peripheral.md` §4 rules 1, 3 and 4.
//!
//! **Keyed by the requesting central's address**, because the GATT server has that address on every
//! request. Two phones walking the beacon list at once must not consume each other's pages, and a
//! read retry must return the same bytes — the cursor advances only on write.
//!
//! Cleared on disconnect, so a reconnecting phone cannot read a stale page from a previous session.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// A central's Bluetooth address, as the GATT request carries it.
pub type PeerAddr = [u8; 6];

#[derive(Debug, Default)]
pub struct ResponseBuffers<T> {
    inner: Mutex<BTreeMap<PeerAddr, T>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl<T: Clone> ResponseBuffers<T> {
    pub fn new() -> ResponseBuffers<T> {
        ResponseBuffers {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replace this peer's buffered response. Called on write — the only thing that advances a walk.
    pub fn store(&self, addr: PeerAddr, value: T) {
        lock(&self.inner).insert(addr, value);
    }

    /// This peer's buffered response, if it wrote one. **Idempotent**: reading does not consume.
    pub fn read(&self, addr: &PeerAddr) -> Option<T> {
        lock(&self.inner).get(addr).cloned()
    }

    /// Forget one peer — called on disconnect.
    pub fn forget(&self, addr: &PeerAddr) -> bool {
        lock(&self.inner).remove(addr).is_some()
    }

    /// Forget one peer only if `pred` holds for what it has, judged under the same lock as the
    /// removal, so nothing can slip in between the look and the forget.
    pub fn forget_if(&self, addr: &PeerAddr, pred: impl FnOnce(&T) -> bool) -> bool {
        let mut inner = lock(&self.inner);
        if inner.get(addr).is_some_and(pred) {
            inner.remove(addr);
            return true;
        }
        false
    }

    pub fn clear(&self) {
        lock(&self.inner).clear();
    }

    pub fn len(&self) -> usize {
        lock(&self.inner).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerAddr = [0xAA; 6];
    const B: PeerAddr = [0xBB; 6];

    #[test]
    fn a_read_before_any_write_is_none() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        assert_eq!(b.read(&A), None, "never a default first page");
    }

    #[test]
    fn reads_are_idempotent_so_a_retry_is_safe() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        b.store(A, vec![1, 2, 3]);
        assert_eq!(b.read(&A), Some(vec![1, 2, 3]));
        assert_eq!(
            b.read(&A),
            Some(vec![1, 2, 3]),
            "the cursor advances only on write"
        );
    }

    #[test]
    fn buffers_are_per_central() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        b.store(A, vec![1]);
        b.store(B, vec![2]);
        assert_eq!(b.read(&A), Some(vec![1]));
        assert_eq!(
            b.read(&B),
            Some(vec![2]),
            "two phones do not share a cursor"
        );
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn a_write_replaces_this_peers_buffer_only() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        b.store(A, vec![1]);
        b.store(B, vec![2]);
        b.store(A, vec![9]);
        assert_eq!(b.read(&A), Some(vec![9]));
        assert_eq!(b.read(&B), Some(vec![2]));
    }

    #[test]
    fn forget_if_keeps_what_the_predicate_spares() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        b.store(A, vec![1]);
        assert!(!b.forget_if(&A, |v| v.is_empty()), "spared");
        assert_eq!(b.read(&A), Some(vec![1]));
        assert!(b.forget_if(&A, |v| !v.is_empty()));
        assert_eq!(b.read(&A), None);
        assert!(!b.forget_if(&B, |_| true), "nothing to forget");
    }

    #[test]
    fn disconnect_clears_this_peers_buffers_only() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        b.store(A, vec![1]);
        b.store(B, vec![2]);
        assert!(b.forget(&A));
        assert_eq!(
            b.read(&A),
            None,
            "a reconnecting phone cannot read a stale page"
        );
        assert_eq!(b.read(&B), Some(vec![2]));
        assert!(!b.forget(&A), "forgetting twice is not an error");
    }

    #[test]
    fn clear_drops_everything() {
        let b: ResponseBuffers<Vec<u8>> = ResponseBuffers::new();
        b.store(A, vec![1]);
        b.store(B, vec![2]);
        b.clear();
        assert!(b.is_empty());
    }
}
