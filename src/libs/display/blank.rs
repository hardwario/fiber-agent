//! Blanking the panel before the device powers off.
//!
//! The `St7920` is constructed inside the display thread and never leaves it
//! (see [`super::monitor::display_loop`]), so nothing else in the process can
//! draw. Without a way to ask, a Viewer-initiated power-off cuts the rails while
//! the last sensor overview is still lit — a monitoring device that looks like
//! it is still monitoring, showing readings that stopped updating the moment it
//! died. `systemctl poweroff` sends SIGTERM, which the app does not catch, so no
//! `Drop` impl gets a chance to clean up either: this is the only hook we have.
//!
//! The signal is three-state rather than a bool because the power-off path needs
//! to know the panel actually went dark before it halts, and because a power-off
//! that fails must put the UI back:
//!
//! ```text
//!   Live ──request()──> Requested ──mark_blanked()──> Blanked
//!    ^                      │                            │
//!    └───────────── cancel() ────────────────────────────┘
//! ```
//!
//! Held in a process-wide static for the same reason as
//! [`crate::libs::eye::state`]: the MQTT command executor is thirteen handles
//! deep already, and there is exactly one display in the process.

use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Normal rendering.
pub const LIVE: u8 = 0;
/// Blank asked for; the display thread has not acted on it yet.
pub const REQUESTED: u8 = 1;
/// Panel cleared and backlight off — safe to cut the rails.
pub const BLANKED: u8 = 2;

/// How often [`BlankSignal::wait_until_blank`] re-checks. Short relative to the
/// display loop's own 50 ms tick, so the wait adds no meaningful latency of its
/// own to a budget measured in hundreds of milliseconds.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The blank request/confirm handshake between the power-off path and the
/// display thread.
///
/// A struct rather than bare statics so the state machine can be unit-tested on
/// a local instance — tests in the same binary run in parallel and would
/// otherwise race each other through the process-wide [`BLANK`].
pub struct BlankSignal {
    state: AtomicU8,
}

impl Default for BlankSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl BlankSignal {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(LIVE),
        }
    }

    /// Ask the display thread to blank the panel.
    pub fn request(&self) {
        // Only from Live: re-requesting an already-confirmed blank would send
        // the display thread back through clear+flush for no reason, and worse,
        // make a subsequent wait_until_blank block for the full timeout on a
        // panel that is already dark.
        let _ = self
            .state
            .compare_exchange(LIVE, REQUESTED, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Return to normal rendering — the power-off did not happen.
    pub fn cancel(&self) {
        self.state.store(LIVE, Ordering::SeqCst);
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    /// Called by the display thread once the panel is dark.
    pub fn mark_blanked(&self) {
        // Requested -> Blanked only. A plain store would let a display tick that
        // was already mid-blank when cancel() landed stamp Blanked back over
        // Live, freezing the UI dark on a device that stayed on.
        let _ = self
            .state
            .compare_exchange(REQUESTED, BLANKED, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Block until the panel is confirmed blank, or `timeout` elapses.
    ///
    /// Returns whether it blanked. Bounded on purpose: if the display thread is
    /// absent (init failed, mid-`supervise` backoff) or wedged, the caller is a
    /// power-off that must proceed regardless.
    pub fn wait_until_blank(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state() == BLANKED {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// The one display in this process.
static BLANK: BlankSignal = BlankSignal::new();

/// Ask the display thread to blank the panel. See [`BlankSignal::request`].
pub fn request_blank() {
    BLANK.request();
}

/// Return to normal rendering. See [`BlankSignal::cancel`].
pub fn cancel_blank() {
    BLANK.cancel();
}

/// Current blanking state: [`LIVE`], [`REQUESTED`] or [`BLANKED`].
pub fn blank_state() -> u8 {
    BLANK.state()
}

/// Called by the display thread once the panel is dark.
pub fn mark_blanked() {
    BLANK.mark_blanked();
}

/// Block until the panel is confirmed blank, or `timeout` elapses.
pub fn wait_until_blank(timeout: Duration) -> bool {
    BLANK.wait_until_blank(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn starts_live() {
        assert_eq!(BlankSignal::new().state(), LIVE);
    }

    #[test]
    fn request_then_blank_is_observed_by_the_waiter() {
        let signal = Arc::new(BlankSignal::new());
        signal.request();
        assert_eq!(signal.state(), REQUESTED);

        let display = signal.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            display.mark_blanked();
        });

        assert!(signal.wait_until_blank(Duration::from_secs(2)));
        handle.join().unwrap();
        assert_eq!(signal.state(), BLANKED);
    }

    #[test]
    fn wait_gives_up_when_nothing_confirms() {
        // The display thread is gone or wedged: the power-off must not hang.
        let signal = BlankSignal::new();
        signal.request();

        let start = Instant::now();
        assert!(!signal.wait_until_blank(Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the wait must be bounded by its timeout"
        );
    }

    #[test]
    fn cancel_returns_to_live_from_either_state() {
        let signal = BlankSignal::new();

        signal.request();
        signal.cancel();
        assert_eq!(signal.state(), LIVE);

        signal.request();
        signal.mark_blanked();
        signal.cancel();
        assert_eq!(
            signal.state(),
            LIVE,
            "a device that stays on must not stay dark"
        );
    }

    #[test]
    fn mark_blanked_from_live_is_a_no_op() {
        // A display tick that was already blanking when cancel() landed must not
        // resurrect the blank on a device that is staying on.
        let signal = BlankSignal::new();
        signal.mark_blanked();
        assert_eq!(signal.state(), LIVE);

        signal.request();
        signal.cancel();
        signal.mark_blanked();
        assert_eq!(signal.state(), LIVE);
    }

    #[test]
    fn request_does_not_reset_a_confirmed_blank() {
        let signal = BlankSignal::new();
        signal.request();
        signal.mark_blanked();
        signal.request();
        assert_eq!(signal.state(), BLANKED);
        assert!(signal.wait_until_blank(Duration::from_millis(0)));
    }
}
