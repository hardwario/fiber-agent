//! Panic containment for the display and button threads.
//!
//! Both threads are long-lived loops that own the only local user interface on
//! the device. Left bare, a panic in either one ends the thread for good:
//! nothing joins the handle, nothing logs it, and the operator is left with a
//! frozen or dark panel and no indication why. Alarm annunciation is not
//! affected — the LEDs and buzzer are driven from `LedMonitor` and the sensor
//! thread — so the hazard is bounded to "cannot read or navigate the local
//! display", but that is still not something to fail silently on a medical
//! device.
//!
//! [`supervise`] contains the panic and restarts the loop; [`lock_recover`] and
//! [`read_recover`] deal with the poisoned locks a panic leaves behind, which is
//! the other half of the problem — see their docs.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::Duration;

/// Backoff granularity. The backoff sleep is chunked at this interval so a
/// shutdown request is still honoured promptly — `DisplayMonitor::drop` only
/// waits 2 s for the thread to finish, which a single 5 s sleep would blow.
const BACKOFF_TICK: Duration = Duration::from_millis(50);

/// Backoff added per consecutive panic.
const BACKOFF_STEP: Duration = Duration::from_millis(100);

/// Backoff ceiling, so a loop that panics on every iteration settles into a
/// slow retry with one log line each time rather than a hot spin.
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Run `body` until it returns `Ok` or `shutdown` is set, restarting after a
/// backoff on either a contained panic or a reported failure.
///
/// `body` returns `Result` rather than `()` so that a loop which *declines to
/// start* is retried too. `display_loop` bails out early when `St7920::new()`
/// fails, and re-running `init()` is exactly the recovery this supervisor
/// provides — treating that early exit as a clean shutdown would stop retrying
/// in the one case where retrying is the whole point, and do it silently.
///
/// Restarts are unbounded on purpose: a deterministic failure becomes a slow,
/// noisy retry loop, which is recoverable and visible in the journal. Giving up
/// after N attempts would put us back at a permanently dead UI, which is the
/// failure mode this exists to remove.
pub fn supervise<F>(name: &str, shutdown: &AtomicBool, mut body: F)
where
    F: FnMut() -> Result<(), String>,
{
    let mut consecutive: u32 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        // AssertUnwindSafe: everything `body` captures is either owned by it or
        // an Arc<Mutex/RwLock> handle. Lock poisoning is handled explicitly by
        // the recover helpers below rather than relied on for correctness, so a
        // half-finished frame cannot leave observably broken state behind.
        let result = std::panic::catch_unwind(AssertUnwindSafe(&mut body));

        // Distinct prefixes: a panic is a bug to chase, a reported failure is
        // usually hardware saying no. The journal should not conflate them.
        let reason = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("FAILED: {}", e),
            Err(payload) => format!("PANIC contained: {}", payload_message(&payload)),
        };

        consecutive += 1;
        eprintln!("[{}] {} (restart #{})", name, reason, consecutive);
        let backoff = (BACKOFF_STEP * consecutive).min(BACKOFF_MAX);
        sleep_interruptible(backoff, shutdown);
    }
}

/// Extract a human-readable message from a panic payload.
fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Sleep up to `total`, waking early if shutdown is requested.
fn sleep_interruptible(total: Duration, shutdown: &AtomicBool) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let chunk = BACKOFF_TICK.min(total - slept);
        thread::sleep(chunk);
        slept += chunk;
    }
}

/// Lock a mutex, recovering the guard if the lock is poisoned.
///
/// Restarting a panicked thread is not enough on its own: if the panic happened
/// while the display state lock was held, that mutex stays poisoned and *every*
/// later `lock()` in both the display and button threads returns `Err` — so the
/// restarted loop would spin and the buttons would go deaf. The state behind
/// these locks is navigation data (current screen, page, cursor) with no
/// invariant that a partial write could turn into something unsafe, so resuming
/// with it is both correct and strictly better than dropping the UI.
pub fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read-lock an `RwLock`, recovering the guard if it is poisoned.
///
/// See [`lock_recover`]. This also replaces the pattern
/// `state.read().unwrap_or_else(|_| state.read().unwrap())`, whose fallback
/// re-locks the still-poisoned lock and so panics for certain.
pub fn read_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write-lock an `RwLock`, recovering the guard if it is poisoned.
///
/// See [`lock_recover`]. The `if let Ok(guard) = lock.write()` shape this
/// replaces is the more dangerous half of the pattern: a skipped *read* costs
/// one stale frame, but a skipped *write* means the update never lands at all,
/// silently and for the rest of the process's life.
pub fn write_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Silence the default panic hook (and its backtrace) for the duration of a
    /// test that panics on purpose, so the output stays readable.
    fn without_panic_output<R>(f: impl FnOnce() -> R) -> R {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = f();
        std::panic::set_hook(previous);
        out
    }

    #[test]
    fn supervise_returns_on_clean_exit() {
        let shutdown = AtomicBool::new(false);
        let mut calls = 0;
        supervise("test", &shutdown, || {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 1, "a clean return must not be restarted");
    }

    #[test]
    fn supervise_restarts_body_after_panic() {
        let shutdown = AtomicBool::new(false);
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_inner = calls.clone();

        without_panic_output(|| {
            supervise("test", &shutdown, move || {
                let n = calls_inner.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    panic!("boom {}", n);
                }
                Ok(())
            });
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two panics then a clean run"
        );
    }

    #[test]
    fn supervise_restarts_body_after_reported_failure() {
        // The display-init case: the loop declines to start rather than
        // panicking, and must still be retried.
        let shutdown = AtomicBool::new(false);
        let mut calls = 0;

        supervise("test", &shutdown, || {
            calls += 1;
            if calls < 3 {
                Err(format!("display init failed (attempt {})", calls))
            } else {
                Ok(())
            }
        });

        assert_eq!(calls, 3, "two reported failures then a clean run");
    }

    #[test]
    fn supervise_stops_reporting_failures_once_shutdown_requested() {
        // A body that never succeeds must not spin forever past a shutdown.
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_inner = shutdown.clone();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_inner = calls.clone();

        supervise("test", &shutdown, move || {
            calls_inner.fetch_add(1, Ordering::SeqCst);
            shutdown_inner.store(true, Ordering::SeqCst);
            Err("still failing".to_string())
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn supervise_stops_when_shutdown_flag_set() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_inner = shutdown.clone();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_inner = calls.clone();

        without_panic_output(|| {
            supervise("test", &shutdown, move || {
                calls_inner.fetch_add(1, Ordering::SeqCst);
                // Ask for shutdown, then panic: the supervisor must not restart.
                shutdown_inner.store(true, Ordering::SeqCst);
                panic!("boom");
            });
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must not restart once shutdown is requested"
        );
    }

    #[test]
    fn supervise_does_not_run_body_when_already_shut_down() {
        let shutdown = AtomicBool::new(true);
        let mut calls = 0;
        supervise("test", &shutdown, || {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 0);
    }

    #[test]
    fn lock_recover_returns_inner_after_poisoning() {
        let mutex = Arc::new(Mutex::new(41));
        let poisoner = mutex.clone();

        without_panic_output(|| {
            let _ = thread::spawn(move || {
                let _guard = poisoner.lock().unwrap();
                panic!("poison it");
            })
            .join();
        });

        assert!(mutex.lock().is_err(), "precondition: lock is poisoned");
        let mut guard = lock_recover(&mutex);
        *guard += 1;
        assert_eq!(*guard, 42, "state stays usable after a contained panic");
    }

    #[test]
    fn read_recover_returns_inner_after_poisoning() {
        let lock = Arc::new(RwLock::new(vec![1, 2, 3]));
        let poisoner = lock.clone();

        without_panic_output(|| {
            let _ = thread::spawn(move || {
                let _guard = poisoner.write().unwrap();
                panic!("poison it");
            })
            .join();
        });

        assert!(lock.read().is_err(), "precondition: lock is poisoned");
        assert_eq!(*read_recover(&lock), vec![1, 2, 3]);
    }

    #[test]
    fn write_recover_returns_inner_after_poisoning() {
        let lock = Arc::new(RwLock::new(vec![1, 2, 3]));
        let poisoner = lock.clone();

        without_panic_output(|| {
            let _ = thread::spawn(move || {
                let _guard = poisoner.write().unwrap();
                panic!("poison it");
            })
            .join();
        });

        assert!(lock.write().is_err(), "precondition: lock is poisoned");
        write_recover(&lock).push(4);
        assert_eq!(*read_recover(&lock), vec![1, 2, 3, 4], "the write must land");
    }
}
